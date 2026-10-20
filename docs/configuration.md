# Configuration and keybindings

FACT reads an optional TOML file. Without one, built-in defaults are used. The
default path is `%LOCALAPPDATA%\FACT\config.toml`; `fact config path` prints the
path actually in use.

```powershell
fact config validate          # parse, validate and compile all bindings
fact config validate --json   # machine-readable report
fact config init             # write a commented starter file
fact config init --force     # replace an existing file
```

Unknown fields, an unknown `config_version`, duplicate keybindings and invalid
values are errors. `fact doctor` also validates the config.

## Paths and environment

Resolution order for the data directory:

1. `--data-dir DIR`
2. `FACT_HOME`
3. `%LOCALAPPDATA%\FACT`
4. `.fact` in the current directory if `LOCALAPPDATA` is unavailable

`--config FILE` overrides only the config file. Relative paths are resolved
against the process working directory.

| Path | Contents |
| --- | --- |
| `config.toml` | This configuration. |
| `sessions.db` | SQLite database. |
| `runtime\daemon.json` | Protected daemon identity and authentication token. |
| `logs\daemon.log` | Daemon log. |
| `logs\client.log` | Client log. |
| `exports\` | Default export destination. |

## Top-level settings

```toml
config_version = 1
prefix = "ctrl+space"
theme = "fact-dark"

scrollback_lines = 8000
min_pane_width = 16
min_pane_height = 5
mouse = true
confirm_close = true
copy_on_select = false

persist_interval_secs = 10
max_sessions = 32
max_frame_cells = 2000000
daemon_log_level = "info"

default_profile = "auto"
```

| Key | Default | Valid values / meaning |
| --- | --- | --- |
| `config_version` | `1` | Must be exactly `1`. |
| `prefix` | `ctrl+space` | Chord that arms FACT commands. May not use the literal word `prefix`. |
| `theme` | `fact-dark` | `fact-dark`, `fact-light`, `dracula`, `nord`, `gruvbox`. |
| `scrollback_lines` | `8000` | 100-100000 retained lines per pane. |
| `min_pane_width` | `16` | At least 12 columns per pane before focused-pane mode. The default fits a 4x2 grid in an 80-column window. |
| `min_pane_height` | `5` | At least 3 rows per pane before focused-pane mode. |
| `mouse` | `true` | Enable mouse capture, clicks and split dragging. |
| `confirm_close` | `true` | Ask before closing a pane, tab or the UI with live shells. |
| `copy_on_select` | `false` | Copy a completed mouse selection automatically. |
| `persist_interval_secs` | `10` | 1-600 seconds between metadata/layout saves. |
| `max_sessions` | `32` | 1-128. |
| `max_frame_cells` | `2000000` | 10000-50000000; upper bound for one frame. |
| `daemon_log_level` | `info` | `trace`, `debug`, `info`, `warn`, `error`. |
| `default_profile` | `auto` | `auto` or a key from `[profiles]`. |

The v0.2 keys `snapshot_interval_secs` and `snapshot_max_bytes` are still accepted
so existing configuration files continue to load, but they are ignored. FACT no
longer captures or persists terminal output.

`F2` cycles themes for the current UI session without writing the config. The
next launch uses the `theme` value again. The command palette also has a
"Cycle color theme" action.

## Profiles

A profile describes one shell command. `auto` is built in and is not a TOML
profile. It selects `pwsh.exe`, then `powershell.exe`, then `cmd.exe`.

```toml
default_profile = "pwsh"

[profiles.pwsh]
command = "pwsh.exe"
args = ["-NoLogo"]
cwd = "~"
title = "PowerShell"

[profiles.cmd]
command = "cmd.exe"

[profiles.wsl]
command = "wsl.exe"
args = ["-d", "Ubuntu", "--cd", "~"]

[profiles.disabled-example]
command = "cmd.exe"
enabled = false
```

| Field | Meaning |
| --- | --- |
| `command` | Executable name or absolute path. Omitted/empty uses the detected shell. |
| `args` | Exact argument list; no shell expansion is performed by FACT. |
| `cwd` | Initial directory. Supports `~` and `%VARIABLE%`. Defaults to the user profile. |
| `env` | Extra environment variables, merged over global `[env]`. |
| `title` | Label shown on the tab strip instead of the executable name. |
| `enabled` | Defaults to `true`. Disabled profiles cannot be selected. |

Profile names are 1-64 ASCII letters, digits, `-`, `_` or `.`. Select one with
`--profile NAME` or by setting `default_profile`. An unknown or disabled profile
is an error.

FACT also sets `TERM=xterm-256color`, `COLORTERM=truecolor`,
`TERM_PROGRAM=FACT`, `TERM_PROGRAM_VERSION` and `FACT=1` unless your profile or
environment already defines them.

## Environment

```toml
[env]
EDITOR = "code --wait"
MY_PROJECT_ROOT = "C:\\src"
```

Global values apply to every pane. Profile values override them. Environment
values are applied to the child process and are never written to logs or the
session database. Do not put secrets in the config file: it is ordinary user-readable
state, not a credential store.

## Keybindings

The prefix is armed for one command. For example, with the default
`ctrl+space`, press `Ctrl+Space`, release, then `K` to open the palette.
The prefix chord itself is not sent to the shell.

Bindings are written as:

- `prefix+KEY` - requires the prefix, e.g. `prefix+k`.
- `ctrl+shift+c`, `alt+enter`, `f1` - direct chords.
- `prefix++` or `ctrl+shift++` - the literal plus key.
- Key names: single characters, `enter`, `tab`, `esc`, `backspace`, `delete`,
  `home`, `end`, `pageup`, `pagedown`, `insert`, arrow keys, `space`, and
  `f1`-`f24`.

```toml
[keybinds]
split_right = "prefix+|"
palette = "ctrl+p"
help = "f1"
```

Every action must have exactly one binding after overrides are applied. Direct
bindings cannot collide with each other or with prefix bindings. A direct key
that is not a FACT binding is sent to the focused shell.

### Default bindings

| Action | Config key | Default |
| --- | --- | --- |
| Split right | `split_right` | Prefix then `\|` |
| Split down | `split_down` | Prefix then `-` |
| Close pane | `close_pane` | Prefix then `X` |
| Close tab / grid | `close_tab` | Prefix then `Shift+X`, or `Alt+T` |
| Restart pane | `restart_pane` | Prefix then `Shift+R` |
| Toggle maximize / full grid | `zoom` | Prefix then `Z` |
| Maximize focused pane | `maximize` | Prefix then `M`, or `Alt+M` |
| Show all panes | `show_all` | Prefix then `N`, or `Alt+N` |
| Minimize pane | `minimize` | Prefix then `Shift+M` |
| Restore minimized panes | `restore` | Prefix then `Shift+U` |
| Focus left/right/up/down | `focus_left`, `focus_right`, `focus_up`, `focus_down` | Prefix then arrow key |
| Next pane | `next_pane` | Prefix then `O` |
| Previous pane | `prev_pane` | Prefix then `I` |
| New tab | `new_tab` | Prefix then `C` |
| Next tab | `next_tab` | Prefix then `Shift+N` |
| Previous tab | `prev_tab` | Prefix then `P` |
| Rename tab | `rename_tab` | Prefix then `Shift+T` |
| Command palette | `palette` | Prefix then `K` |
| Search focused pane | `search` | Prefix then `/` |
| Browse scrollback | `scroll_mode` | Prefix then `[` |
| Switch session | `sessions` | Prefix then `S` |
| Detach, keep shells | `detach` | Prefix then `D` |
| Help | `help` | `F1` |
| Copy selection | `copy` | `Ctrl+Shift+C` or `Ctrl+C` with a selection |
| Paste | `paste` | `Ctrl+Shift+V` or `Ctrl+V` |
| Quit UI | `quit` | Prefix then `Shift+Q` |
| Swap with next pane | `swap_pane` | Prefix then `W` |
| Move pane to next tab | `move_pane` | Prefix then `Shift+O` |

Additional interaction that is not a configurable chord:

- `F2` cycles themes unless you bind `F2` to another action.
- `Alt+M` maximizes the focused pane and `Alt+N` shows the full grid without
  using the prefix. `Alt+T` terminates the active grid and all of its panes after
  confirmation. `Ctrl+M` is Enter in terminal input, so it cannot be used as a
  direct shortcut.
- Prefix then `[` enters scroll mode without sending normal keys to the
  application. Up/Down scroll one line, PgUp/PgDn scroll one page, Home/End jump
  to the top or bottom, and Esc or `q` exits. In a shell this browses FACT's
  scrollback. In an alternate-screen app it becomes app scroll: PgUp/PgDn,
  Up/Down and Home/End are forwarded to the app so its own transcript moves.
- Full-screen applications (Claude Code, Codex and OpenCode by default) own the
  alternate screen, which has no terminal scrollback. FACT keeps the rail
  visible with a centred grip and paging caps. Clicking or dragging the rail,
  wheeling over it, or using Shift+wheel pages the app; if the app requested
  mouse reporting, FACT synthesizes wheel events at the centre of its viewport.
- For true terminal history and a position thumb, launch an agent in inline
  mode: `codex --no-alt-screen`, `/tui default` in Claude Code, or
  `OTUI_USE_ALTERNATE_SCREEN=false` for OpenCode.
- In a window too small for the configured `min_pane_width`/`min_pane_height`,
  FACT normally shows only the focused pane. `Alt+N` or the `show_all` action
  keeps the whole grid visible even below those limits; `Alt+M` maximizes again.
- Drag a split gutter with the mouse to resize.
- Drag in pane content to select text; `Shift`+drag selects even when the
  application has requested mouse reporting.
- Click a pane body to focus it. A single-tab session has no tab strip; with two
  or more tabs, click a tab to select it and click the trailing `+` to add one.
- Right-click a pane for split, zoom, minimize, new tab, rename, search, restart,
  close and copy actions. Applications that request mouse reporting receive the
  click instead; use the keyboard actions in that case.
- Click or drag the scrollbar to move through scrollback. In a full-screen pane
  the same rail pages the application instead. The mouse wheel is forwarded to
  the focused application when it requested mouse reporting.
- When panes are minimized, a small `▾ N` tray appears at the bottom-right; the
  same tray is clickable to restore them.
- `Ctrl+C` copies a selection, `Ctrl+V` pastes, and `Ctrl+Insert` /
  `Shift+Insert` are accepted aliases outside the remappable action table.

`F1` help and the palette show the bindings currently compiled from this file,
including your overrides.

## Examples

A three-profile setup with a custom prefix and a longer autosave interval:

```toml
config_version = 1
prefix = "ctrl+b"
theme = "nord"
default_profile = "pwsh"
persist_interval_secs = 15

[profiles.pwsh]
command = "pwsh.exe"
args = ["-NoLogo"]
title = "PowerShell 7"

[profiles.cmd]
command = "cmd.exe"
title = "Command Prompt"

[profiles.ubuntu]
command = "wsl.exe"
args = ["-d", "Ubuntu"]
title = "Ubuntu"

[env]
EDITOR = "code --wait"

[keybinds]
palette = "ctrl+p"
search = "ctrl+f"
```

Validate before launching:

```powershell
fact config validate --json
fact doctor
fact --profile ubuntu --grid 4
```
