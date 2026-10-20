# Architecture decision records

These records capture the choices that shape FACT's process model, security
boundary and persistence behavior. They describe the current design, not a
promise that alternatives are impossible.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-per-user-daemon.md) | Per-user daemon owns all shells | Amended by 0007 |
| [0002](0002-local-named-pipe-security.md) | Raw Windows named pipes with DACL and token authentication | Accepted |
| [0003](0003-visual-cold-recovery.md) | Alacritty terminal model and visual-only cold recovery | Superseded by 0007 |
| [0004](0004-binary-split-tree-and-frames.md) | Binary split tree with incremental cell frames | Accepted |
| [0005](0005-sqlite-and-json-export.md) | SQLite persistence with versioned JSON export | Amended by 0007 |
| [0006](0006-single-threaded-tui.md) | Single-threaded Ratatui loop with a separate event reader | Accepted |
| [0007](0007-fresh-shells-no-replay.md) | Saved layouts reopen with fresh shells; no output replay | Accepted |

For the resulting system, see [../architecture.md](../architecture.md) and
[../security.md](../security.md).
