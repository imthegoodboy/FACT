# fact-terminal

FACT is a lightweight Windows terminal workspace: real ConPTY shells in tabs,
splits and grids inside one ordinary terminal window. One small native
`fact.exe`, no daemon, no saved state, and keyboard handling that AI agent CLIs
(Claude Code, Codex, OpenCode, Copilot) understand: Shift+Enter, Ctrl+Enter
and Ctrl+Backspace reach them intact.

```powershell
npm install -g fact-terminal

fact                  # one shell
fact --grid 4         # four shells in a grid
fact doctor           # check the terminal, config, PATH and agent CLIs
fact wallpaper set C:\Pictures\sky.jpg   # background image (Windows Terminal)
```

Inside FACT the prefix is `Ctrl+Space`; press `F1` for every shortcut.
Alt+drag moves a pane, dragging a tab reorders tabs, and double-clicking a
divider balances the panes.

The package contains the prebuilt Windows x64 `fact.exe`. npm runs it
directly, without Node.js in between.

Documentation, source and releases: https://github.com/imthegoodboy/FACT
