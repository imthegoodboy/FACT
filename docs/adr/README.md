# Architecture decision records

These records capture the choices that shape FACT's process model, terminal
ownership and rendering behavior. They describe the current design, not a
promise that alternatives are impossible.

| ADR | Decision | Status |
| --- | --- | --- |
| [0004](0004-binary-split-tree-and-frames.md) | Binary split tree with incremental cell frames | Accepted |
| [0006](0006-single-threaded-tui.md) | Single-threaded Ratatui loop with a separate event reader | Accepted |
| [0008](0008-single-process-no-persistence.md) | One process owns the UI, engine and shells; no persistence | Accepted |

For the resulting system, see [../architecture.md](../architecture.md) and
[../security.md](../security.md).
