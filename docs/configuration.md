# Configuration and keybindings

FACT reads an optional TOML file. Without one, built-in defaults are used. The
default path is `%LOCALAPPDATA%\FACT\config.toml`; `fact config path` prints the
path actually in use. A normal FACT launch never writes this file.

```powershell
fact config validate          # parse, validate and compile all bindings
fact config validate --json   # machine-readable report
fact config init             # write a commented starter file
fact config init --force     # replace an existing file
```

Unknown fields, an unknown `config_version`, duplicate keybindings and invalid
values are errors. A launch with an invalid configuration fails before any
shell starts.

## Paths and environment

Resolution order for the base directory:

1. `FACT_HOME`
2. `%LOCALAPPDATA%\FACT`
3. `.fact` in the current directory if `LOCALAPPDATA` is unavailable

`--config FILE` overrides the configuration file. Relative paths are resolved
against the process working directory.

| Path | Contents |
| --- | --- |
| `config.toml` | This configuration (unless `--config` names another file). |
| `wallpaper\` | Only after `fact wallpaper set`: FACT's copy of an image and its `settings.json`. Always in the base directory, even with `--config`. Videos stay where they are. |
| No other paths | FACT creates no database, log, runtime identity, saved layout or exports. |

FACT does not accept a `--data-dir` option. The base directory only holds the
optional configuration file and the wallpaper; the rest of the process is in
memory.

## Top-level settings

```toml
config_version = 2
prefix = "ctrl+space"
theme = "fact-dark"

scrollback_lines = 8000
min_pane_width = 16
min_pane_height = 5
mouse = true
confirm_close = true
copy_on_select = false
alt_shortcuts = false
background = "theme"
wallpaper_tab = true

default_profile = "auto"

[colors]
background = "#14213d"
foreground = "#fca311"
```

| Key | Default | Valid values / meaning |
| --- | --- | --- |
| `config_version` | `2` | Must be exactly `2`. Version 1 files and removed daemon settings are rejected. |
| `prefix` | `ctrl+space` | Chord that arms FACT commands. May not include the literal word `prefix`. |
| `theme` | `fact-dark` | `fact-dark`, `fact-light`, `dracula`, `nord`, `gruvbox`. |
| `scrollback_lines` | `8000` | 100-100000 retained lines per pane. |
| `min_pane_width` | `16` | At least 12 columns per pane before focused-pane mode. The default fits a 4x2 grid in an 80-column window. |
| `min_pane_height` | `5` | At least 3 rows per pane before focused-pane mode. |
| `mouse` | `true` | Enable mouse capture, clicks, selection and split dragging. |
| `confirm_close` | `true` | Ask before closing a pane or tab. Quitting FACT never prompts and closes every shell. |
| `copy_on_select` | `false` | Copy a completed mouse selection automatically. |
| `alt_shortcuts` | `false` | `true` enables the direct `Alt+M` / `Alt+N` / `Alt+T` shortcuts. Off by default because Claude Code, readline and other apps use those chords. |
| `background` | `theme` | `theme` paints the theme background. `transparent` leaves default backgrounds to the host terminal so its background image or acrylic shows through. `fact --transparent` does the same for one launch. |
| `wallpaper_tab` | `true` | In Windows Terminal, a plain `fact` opens a FACT tab where a picture wallpaper is drawn at full resolution. `false` always starts in the current tab, like `fact --here`. |
| `default_profile` | `auto` | `auto` or a key from `[profiles]`. |

Prefix then `T` (the `cycle_theme` action) cycles themes for the current run
without writing the config. The next launch uses the `theme` value again.

### Your own colors

The optional `[colors]` table replaces theme colors with your own, as
`"#rrggbb"` (or `"#rgb"`). Any you leave out keep the theme's, and they apply
on top of every theme, including while cycling.

| Key | What it colors |
| --- | --- |
| `background` | Panes, tab strip and status line (everything that used the theme's background). |
| `foreground` | Terminal text. Programs asking for the default colors (OSC 10/11) get yours. |
| `accent` | Cursor and highlights. |
| `selection` | Selected text. |

An invalid color is reported by `fact config validate` and stops FACT from
starting, like any other config error.

There are no persistence, session-limit or daemon-log settings. Old v0.3 keys
such as `max_sessions`, `persist_interval_secs` and `daemon_log_level` are
unknown fields and are rejected.

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

[profiles.claude]
command = "claude"
title = "Claude Code"

[profiles.disabled-example]
command = "cmd.exe"
enabled = false
```

| Field | Meaning |
| --- | --- |
| `command` | Program name or path. A bare name is looked up in the folders on PATH, trying PATHEXT extensions (`claude` finds `claude.cmd`), but never in the current folder; batch files run through `cmd.exe`. Omitted uses the detected shell. |
| `args` | Exact argument list; no shell expansion is performed by FACT. |
| `cwd` | Initial directory. Supports `~` and `%VARIABLE%`. Defaults to the user profile. |
| `env` | Extra environment variables, merged over global `[env]`. |
| `title` | Label shown on the tab strip instead of the executable name. |
| `enabled` | Defaults to `true`. Disabled profiles cannot be selected. |

Profile names are 1-64 ASCII letters, digits, `-`, `_` or `.`. Select one with
`--profile NAME` or by setting `default_profile`. An unknown or disabled profile
is an error. `--profile` applies to the initial shells in a `--grid`; new tabs
use `default_profile`.

FACT sets `FACT=1`, `FACT_VERSION`, an informational `FACT_PANE` and, unless
already defined, `COLORTERM=truecolor`. It deliberately leaves `TERM` and
`TERM_PROGRAM` as inherited: agent CLIs choose their keyboard protocol from
them (see [Agent CLIs and keyboard protocols](#agent-clis-and-keyboard-protocols)).

## Environment

```toml
[env]
EDITOR = "code --wait"
MY_PROJECT_ROOT = "C:\\src"
```

Global values apply to every pane. Profile values override them. Environment
values are applied to child processes; FACT writes no runtime state. Do not put
secrets in the config file: it is ordinary user-readable configuration, not a
credential store. Values are present in the FACT process and its child
environment while FACT runs.

## Keybindings

The prefix is armed for one command. For example, with the default
`ctrl+space`, press `Ctrl+Space`, release, then `K` to open the palette.
A single prefix press is not sent to the shell; pressing it twice sends it.

Bindings are written as:

- `prefix+KEY` - requires the prefix, e.g. `prefix+k`.
- `none` - no key at all, which frees the default chord for the application.
- `ctrl+shift+c`, `alt+enter`, `f1` - direct chords.
- `prefix++` or `ctrl+shift++` - the literal plus key.
- Key names: single characters, `enter`, `tab`, `esc`, `backspace`, `delete`,
  `home`, `end`, `pageup`, `pagedown`, `insert`, arrow keys, `space`, and
  `f1`-`f24`.

```toml
[keybinds]
split_right = "prefix+|"
palette = "ctrl+p"
help = "none"          # F1 now reaches the focused app
```

Every action has at most one binding after overrides are applied. Direct
bindings cannot collide with each other or with prefix bindings. A direct key
that is not a FACT binding is sent to the focused shell. F1 help and the palette
show the bindings currently compiled from this file.

### Default bindings

| Action | Config key | Default |
| --- | --- | --- |
| Split right | `split_right` | Prefix then `\|` |
| Split down | `split_down` | Prefix then `-` |
| Close pane | `close_pane` | Prefix then `X` |
| Close tab / grid | `close_tab` | Prefix then `Shift+X` |
| Restart pane | `restart_pane` | Prefix then `Shift+R` |
| Toggle maximize / full grid | `zoom` | Prefix then `Z` |
| Maximize focused pane | `maximize` | Prefix then `M` |
| Show all panes | `show_all` | Prefix then `N` |
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
| Help | `help` | `F1` |
| Copy selection | `copy` | `Ctrl+Shift+C` |
| Paste | `paste` | `Ctrl+Shift+V` |
| Quit FACT, close shells | `quit` | Prefix then `Shift+Q` |
| Swap with next pane | `swap_pane` | Prefix then `W` |
| Move pane to next tab | `move_pane` | Prefix then `Shift+O` |
| Move the focused pane's edge left/right/up/down | `resize_left`, `resize_right`, `resize_up`, `resize_down` | Prefix then `Shift`+arrow key |
| Balance pane sizes in the tab | `balance` | Prefix then `=` |
| Cycle color theme | `cycle_theme` | Prefix then `T` |
| Hide / show the wallpaper (pauses a video) | `toggle_wallpaper` | Prefix then `B` |

Additional interaction that is not a remappable action:

- With `alt_shortcuts = true`, `Alt+M` maximizes the focused pane, `Alt+N`
  shows the full grid and `Alt+T` closes the active tab after confirmation. They
  are off by default, so those chords reach the application.
- Press the prefix twice to send the prefix chord itself to the application
  (for example PSReadLine's `Ctrl+Space` completion menu).
- `Ctrl+C` copies a selection; with no selection it is sent to the shell as
  normal terminal input. `Ctrl+V` pastes clipboard text. When the clipboard
  holds an image or other non-text data, `Ctrl+V` goes to the application
  instead, so agents can attach the image themselves. `Ctrl+Insert` and
  `Shift+Insert` are accepted as clipboard aliases. These aliases follow their
  actions: with `paste = "none"`, `Ctrl+V` and `Shift+Insert` go to the
  application; with `copy = "none"`, so do `Ctrl+C` and `Ctrl+Insert`.
- `Esc` with F1 help open reaches the focused pane (help stays open; `F1` or
  `q` closes it), so an agent can always be interrupted.
- Prefix then `[` enters scroll mode without sending normal keys to the
  application. Up/Down scroll one line, PgUp/PgDn scroll one page, Home/End jump
  to the top or bottom, and Esc or `q` exits. In a shell this browses FACT's
  scrollback. In an alternate-screen app it becomes app scroll: PgUp/PgDn,
  Up/Down and Home/End are forwarded to the app so its own transcript moves.
- Full-screen applications, including agent CLIs configured for that mode, own
  the alternate screen, which has no terminal scrollback. FACT adds no scrollbar
  to alternate-screen or mouse-controlled panes. Wheel and Shift+wheel still
  page full-screen apps: mouse-aware apps receive wheel reports at the original
  cell, and keyboard-only apps receive PageUp/PageDown, avoiding prompt-history
  arrows. Prefix scroll mode targets its pane even if the pointer moves away,
  synthesizing mouse-aware wheel reports at that pane's centre.
- For true terminal history and a position thumb, launch an agent in inline
  mode: `codex --no-alt-screen`, `/tui default` in Claude Code, or
  `OTUI_USE_ALTERNATE_SCREEN=false` for OpenCode.
- In a window too small for the configured `min_pane_width`/`min_pane_height`,
  FACT normally shows only the focused pane. The `show_all` action (prefix then
  `N`, or `Alt+N` with `alt_shortcuts = true`) keeps the whole grid visible even
  below those limits; `maximize` (prefix then `M`) focuses one pane again.
- Drag a split gutter with the mouse to resize; double-click a gutter to
  balance every pane in the tab.
- `Alt`+drag a pane onto another pane to swap them, or onto a tab in the tab
  strip to move it there. The drop target is outlined while you drag.
- Drag a tab along the tab strip to reorder tabs.
- Drag in pane content to select text; `Shift`+drag selects even when the
  application has requested mouse reporting. Double-click selects a word and
  triple-click selects a visual line. The selected pane's display pauses while
  its process continues running; copied text comes from that snapshot. Esc,
  typing or scrolling resumes live output.
- Right-click copies a current selection. With no selection it pastes into a
  shell, or is forwarded as a right-click to an application that requested mouse
  reporting. `Shift`+right-click always opens FACT's pane menu.
- Click a pane body to focus it. A single-tab workspace has no tab strip; with
  two or more tabs, click a tab to select it and click the trailing `+` to add
  one.
- Hover near a shell pane's right edge to reveal its history scrollbar, then
  click or drag it. Moving away hides it after any active drag ends; losing
  terminal focus clears the hover. FACT adds no scrollbar to alternate-screen or
  mouse-controlled apps. Their wheel events still reach the application, with
  page-key fallback for keyboard-only full-screen apps.
- When panes are minimized, a small `▾ N` tray appears at the bottom-right; the
  same tray is clickable to restore them.

## Agent CLIs and keyboard protocols

FACT speaks the same keyboard protocols as modern terminals, so agent CLIs see
the keys you press:

- **win32-input-mode.** Windows' ConPTY asks its host for Windows Terminal's
  win32-input-mode. FACT honors it: modified keys travel as complete Windows
  key events, so console programs (PowerShell's PSReadLine, Codex) can tell
  `Shift+Enter`, `Ctrl+Enter` and `Ctrl+Backspace` apart from their plain
  versions. Ordinary text is unchanged.
- **Kitty keyboard protocol.** Claude Code, OpenCode and GitHub Copilot ask for
  it. FACT answers the query and, while an app has it enabled, sends modified
  `Enter`, `Tab` and `Backspace` as `CSI code ; modifiers u`. `Shift+Enter`
  then inserts a new line instead of submitting the prompt.
- FACT intercepts nothing outside the prefix by default except `F1` help and the
  clipboard keys above, and each of those can be set to `none`.

Inside Windows Terminal, Claude Code recognises the host from the inherited
environment and enables the kitty protocol immediately. Under other hosts it
probes for it; `Ctrl+J`, `Alt+Enter` or `\` then `Enter` also insert a new
line there.

## Wallpaper

FACT paints a picture or a looping video behind every pane itself, so it shows
in any terminal: Windows Terminal, the classic console, VS Code and others.

```powershell
fact wallpaper set C:\Pictures\sky.jpg               # 35% strength, cover the window
fact wallpaper set .\forest.png --opacity 20 --fit fit
fact wallpaper set C:\Videos\rain.mp4                # loops; needs ffmpeg
fact wallpaper set --opacity 50                      # change the current one in place
fact wallpaper                                       # show the current wallpaper
fact wallpaper clear                                 # remove it again
```

Running FACT windows switch at once when you set, change or clear the
wallpaper, from any terminal (a FACT pane included); there is nothing to
restart. Prefix then `B` hides the wallpaper and pauses a video, and shows it
again. A new file keeps the current opacity and fit unless you give
`--opacity` or `--fit`; `set` without a file changes only those.

- **Images** (png, jpg, jpeg, gif, bmp, webp) are copied to the `wallpaper\`
  folder in FACT's data folder (`%LOCALAPPDATA%\FACT\wallpaper\`, or under
  `FACT_HOME` when set), even with `--config`. Animated GIFs loop.
- **Videos** (mp4, mkv, webm, mov, avi, m4v, wmv, flv) play from where they are,
  in a loop, at their own colors. ffmpeg decodes them and streams frames scaled
  to the window's cell grid at 24 fps, the finest detail a terminal can show
  behind text. Install ffmpeg with `winget install ffmpeg`, or point
  `FACT_FFMPEG` at `ffmpeg.exe`.
- Where FACT paints the wallpaper itself it uses four pixels per cell (2x2
  quadrant block characters) in Windows Terminal, VS Code and WezTerm, and two
  (`▀` half blocks) in the classic console, whose fonts may lack quadrants. The
  image fades toward the theme's background by `--opacity` (5-100 or 0.05-1.0)
  so text stays readable. Only cells at the default background are painted:
  the cursor, selections, panels and program colors stay on top.
- Animation pauses while the FACT window is unfocused, so a video costs nothing
  in the background.
- `--fit` accepts `fill` (default, cover and crop), `fit` (letterbox),
  `stretch` and `center` (a small image keeps its size, centered; a large one
  is shrunk to fit).
- `background = "transparent"` or `--transparent` turns the painted wallpaper
  off so the terminal's own background shows.

For images, `set` also writes a Windows Terminal *fragment*
(`%LOCALAPPDATA%\Microsoft\Windows Terminal\Fragments\FACT\fact.json`) that adds
a "FACT" profile with the image, where Windows Terminal draws it at full
resolution and FACT stays transparent on top. A plain `fact` typed in any other
Windows Terminal tab opens a FACT tab with that profile; `fact --here` (or
`wallpaper_tab = false`) starts in the current tab instead. `wt -p FACT` and the FACT entry in the tab menu open it
too. Your own
`settings.json` is never edited; FACT only updates its timestamp so Windows
Terminal reloads at once. Set `FACT_WT_FRAGMENTS_DIR` for a portable Windows
Terminal.

With `background = "transparent"` (or `--transparent`) FACT paints no wallpaper
and leaves every default background transparent, so a background image or
acrylic configured in your terminal shows through. A FACT started inside a
transparent or wallpapered FACT pane becomes transparent too, so nested
workspaces never cover the outer wallpaper.

## Examples

A three-profile setup with a custom prefix:

```toml
config_version = 2
prefix = "ctrl+b"
theme = "nord"
default_profile = "pwsh"

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
fact --profile ubuntu --grid 4
```
