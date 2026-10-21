# 0006: Single-threaded TUI with a separate event reader

- **Status:** Accepted
- **Date:** 2026-10-19

## Context

Ratatui applications typically have an input/render loop and one or more
producers. FACT has three asynchronous sources: crossterm input, engine frames
from a blocking in-process channel, and local timers/status expiration. The UI
must remain responsive while a shell produces output, and the reader thread must
stop cleanly when FACT exits.

A fully async runtime would add a large dependency for a single event loop.
Spawning a thread per frame would create ordering and teardown problems.

## Decision

Keep the TUI event loop single-threaded:

- crossterm events are polled with a short timeout
- engine frames arrive through a dedicated event-stream reader thread
- the reader forwards decoded `ServerMessage` values over a bounded standard
  channel
- an `AtomicBool` cancellation flag plus dropping the channel lets the UI stop
  the reader on exit without waiting forever
- all application state and rendering happen on the main thread
- the client sends a separate synchronous control request for each action and
  applies the resulting frame

The engine also runs a single owner thread and pushes frames; the client never
polls terminal state directly.

## Consequences

- No async runtime and no shared mutable UI state.
- Frame ordering is preserved by the event stream and sequence numbers.
- Slow rendering can delay input, but does not block the engine; the engine drops
  frames when the UI's bounded frame channel is full.
- Reader teardown is explicit on exit; there is no session switching to manage.
- Frames are the source of truth; optimistic local layout edits could diverge and
  are avoided.

## Alternatives considered

- **Async runtime:** more dependencies for one event loop and one frame channel.
- **Reader thread per pane:** excessive threads and complex lifetime handling.
- **Polling the engine from the render loop:** higher latency and unnecessary
  request traffic.
- **Multi-threaded rendering:** state sharing and terminal corruption risks
  without a clear performance need.
