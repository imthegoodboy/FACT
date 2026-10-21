# README screenshots

These images show FACT 0.3.3 running in real Windows ConPTY sessions. The
documentation capture launches eight independent PowerShell processes in an
isolated temporary FACT home, drives the actual keyboard/mouse controls, and
captures the resulting terminal cell grid. The PNG renderer preserves that
content and its colors using Consolas; it adds only black padding, with no
invented window frame. PIDs and shell versions reflect the capture machine. The demo deliberately
configures a `Ctrl+B` prefix for its driven input; the default is `Ctrl+Space`.

The history example generates numbered lines locally. No model request, agent
conversation, clipboard content or personal session is used.

Regenerate on Windows from the repository root:

```powershell
cargo build --release --bin fact
$env:FACT_SMOKE_EXE = (Resolve-Path target/release/fact.exe).Path
$env:FACT_SCREENSHOT_DIR = Join-Path (Get-Location) 'target/readme-captures'
cargo test --release -p fact-cli --test tui capture_readme_screens -- --ignored --nocapture
# Optional documentation dependency; it is not required to build or run FACT.
python -m pip install Pillow
python scripts/render-readme-captures.py target/readme-captures docs/screenshots
Remove-Item Env:FACT_SMOKE_EXE, Env:FACT_SCREENSHOT_DIR
```

Review each PNG before committing. The capture scenario is ignored by normal
test runs; it is a documentation tool rather than a CI screenshot comparison.
