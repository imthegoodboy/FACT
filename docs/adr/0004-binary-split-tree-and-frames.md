# 0004: Binary split tree with incremental cell frames

- **Status:** Accepted
- **Date:** 2026-10-19

## Context

FACT needs tabs, nested horizontal/vertical splits, grids, focus, zoom,
minimization, move and swap. It also streams full terminal screens to the UI at
interactive rates. Sending a complete frame for every pane on every tick would
waste bandwidth and CPU, especially with eight or more busy shells.

Layouts could be represented as a flat list of rectangles or as a tree. Rendering
could be client-side or server-side.

## Decision

Represent each tab as a binary split tree. Leaves are pane IDs; internal nodes
carry an axis, a normalized ratio and children. Grids are built as balanced trees.
Layout produces one-cell gutters and reserves the rightmost column of each pane
for its scrollbar; there is no pane title row. The daemon owns the tree; the
client renders the resulting rectangles.

Frame delivery:

- build frames on a 33 ms daemon tick
- include session/layout metadata only when relevant
- include a pane's cells only when its version changed since the last frame sent
  to that subscriber
- send the full cell buffer on layout changes or when a pane first appears
- increment a pane version only when terminal cells change
- drop frames for a slow subscriber rather than blocking the owner thread

## Consequences

- Layout operations are small tree transformations and are easy to persist.
- Pane geometry is deterministic and testable.
- The client must maintain per-pane cell buffers and tolerate skipped frames.
- The protocol needs layout epochs, pane versions, detach generations and frame
  sequence numbers.
- Bounded subscriber channels protect terminal processing from a stalled UI.
- Tiny panes switch to focused-pane rendering instead of producing unreadable
  one-column terminals.

## Alternatives considered

- **Flat rectangle list:** simpler rendering, but resize/move/swap and nested
  splits become ad hoc.
- **Client-owned layout:** would diverge across multiple clients and complicate
  daemon-side persistence.
- **Full frame every tick:** simple but unnecessarily expensive and prone to
  backpressure.
