# Install, uninstall and releases

FACT releases are per-user Windows x64 builds. No installer service, MSI or
administrator rights are involved.

## Install a release

Download the zip and `SHA256SUMS.txt` from the
[releases page](https://github.com/imthegoodboy/FACT/releases), then:

```powershell
# Recommended: download and install the latest release through the script.
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1

# Pin a version:
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 -Version v0.3.0

# Custom location, no PATH change:
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 `
  -InstallDir C:\Tools\FACT -NoPath
```

The script:

1. resolves the latest GitHub release (or the requested tag),
2. downloads `fact-vX.Y.Z-x86_64-pc-windows-msvc.zip`,
3. downloads `SHA256SUMS.txt` and refuses to continue on a missing line or hash
   mismatch,
4. extracts `fact.exe` and verifies its reported version,
5. stops an existing daemon before replacing the binary,
6. copies to `%LOCALAPPDATA%\Programs\FACT`,
7. adds that directory to the **user** PATH unless `-NoPath` is given.

Existing sessions remain in `%LOCALAPPDATA%\FACT`. A replaced daemon reopens the
saved layout with fresh shells on the next launch. Previous processes and old
screen output are gone by design; no recovery modal is shown.

v0.3.0 uses wire protocol 5. The installer stops the previous daemon before
replacing the binary and removes a dead daemon identity left by a crash or an
older version. A v0.3.0 client never attaches to a daemon speaking a different
protocol: if no process owns the old pipe it clears the stale identity and starts
the current daemon, and if the old daemon is still alive it reports a clear
stop-and-retry error. If you replace `fact.exe` manually, run `fact shutdown`
with the old binary first.

To install a local build:

```powershell
cargo build --release --bin fact
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 `
  -BinaryPath .\target\release\fact.exe -Force
```

Local builds do not have a release checksum. The script says so explicitly.

## Verify a download manually

```powershell
Get-FileHash .\fact-v0.3.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256
Get-Content .\SHA256SUMS.txt
```

The zip's SHA-256 must match the line for that filename. The release workflow
generates both files in the same job, after the gates pass.

## Uninstall

```powershell
# Remove the program and user PATH entry; keep sessions/config/logs.
powershell -ExecutionPolicy Bypass -File .\scripts\uninstall.ps1

# Also remove %LOCALAPPDATA%\FACT (default data root). Irreversible.
powershell -ExecutionPolicy Bypass -File .\scripts\uninstall.ps1 -PurgeData

# Custom install or data location:
powershell -ExecutionPolicy Bypass -File .\scripts\uninstall.ps1 `
  -InstallDir C:\Tools\FACT -DataDir D:\fact-data -PurgeData
```

The uninstaller stops the daemon, removes only `fact.exe` from the install
directory, removes the exact user PATH entry and keeps unrelated files. With
`-PurgeData`, it removes `FACT_HOME` if set, otherwise `%LOCALAPPDATA%\FACT`, or
the explicit `-DataDir`. It refuses to purge a drive root, the user profile
root, an ancestor of the profile, or a non-empty directory without FACT data
markers.

## Release process

1. Merge all changes to `main` through a pull request. CI must pass on the PR.
2. Run the local gates on Windows 11:

   ```powershell
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace --no-fail-fast
   .\scripts\test-install.ps1 -FactExe .\target\debug\fact.exe
   ```

3. Complete the manual smoke checklist in
   [testing.md](testing.md#manual-smoke-check), including a real detach/reattach
   PID check and a saved-layout restart with fresh shell PIDs.
4. Ensure `Cargo.toml`'s workspace version matches the intended tag
   (`X.Y.Z` -> `vX.Y.Z`). Update `CHANGELOG.md`.
5. Create the annotated tag on the verified `main` commit and push it:

   ```powershell
   git switch main
   git pull --ff-only
   git tag -a v0.3.0 -m "FACT v0.3.0"
   git push origin v0.3.0
   ```

6. The `Release` workflow runs. Its `verify` job enforces:

   - the tag matches the workspace version,
   - the tagged commit is an ancestor of `origin/main`,
   - `cargo fmt --all -- --check`,
   - `cargo clippy --workspace --all-targets -- -D warnings`,
   - `cargo test --workspace --no-fail-fast`.

   Only then does the `package` job build `--release --locked`, smoke-test the
   binary and TUI, exercise the installer download path offline, create the zip
   and `SHA256SUMS.txt` and upload the workflow artifact. It creates a **draft**
   GitHub release, verifies that both expected assets are attached, and only
   then publishes the draft.
7. Verify the published assets from a clean directory:

   ```powershell
   gh release view v0.3.0 --repo imthegoodboy/FACT
   gh release download v0.3.0 --repo imthegoodboy/FACT
   Get-FileHash .\fact-v0.3.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\SHA256SUMS.txt
   ```

8. Extract the downloaded zip and run its `scripts\install.ps1`, not a script
   from a different checkout. Run `fact doctor`. Then drive the downloaded
   binary through the real-ConPTY TUI smoke test:

   ```powershell
   .\scripts\test-install.ps1 -FactExe .\fact.exe
   $env:FACT_SMOKE_EXE = "$env:LOCALAPPDATA\Programs\FACT\fact.exe"
   cargo test -p fact-cli --test tui -- --nocapture
   Remove-Item Env:\FACT_SMOKE_EXE
   ```

   The main smoke test launches eight shells, types into a pane and detaches,
   verifying that all eight panes remain live. The
   `tui_saved_layout_starts_fresh_shell_without_recovery_screen` test then stops
   the daemon and resumes to check for a fresh shell PID, no old output and no
   recovery modal; `scroll_mode_reads_history_without_sending_keys_to_the_shell`
   covers keyboard scrolling. Also perform the manual checks in
   [testing.md](testing.md#manual-smoke-check) for the final release sign-off.

Do not create a release manually. A release without the gate run is not a FACT
release.

## Artifact contents

```text
fact-v0.3.0-x86_64-pc-windows-msvc/
  fact.exe
  README.md
  LICENSE
  SECURITY.md
  CHANGELOG.md
  scripts/
    install.ps1
    uninstall.ps1
    test-install.ps1
  docs/
    ...
```

`SHA256SUMS.txt` contains the SHA-256 of the zip.

## Versioning and rollback

- Use semantic versioning. A tag is `v` plus the workspace version.
- A tag is immutable once its release is announced. If verification catches a
  packaging defect before announcement, withdraw the release and tag, fix the
  cause, rerun every gate and recut the same version. Record the withdrawal where
  users could have seen it. After announcement, do not move the tag or
  force-push; publish a new patch release with the fix.
- If the workflow fails after creating the draft, the draft is not a release.
  Inspect it, delete the incomplete draft, fix the cause and publish a new patch
  tag. Never publish a draft whose assets were not produced by a green gate run.
- A bad release can be marked as a pre-release or removed from the releases page,
  but users who already downloaded it are not affected retroactively. Rotate any
  credentials that may have been exposed.
- Release notes are generated by GitHub from commits and pull requests; link the
  migration or breaking-change notes there when needed.

## Compatibility statement

v0.3.0 targets Windows 11 x64. Windows 10 1809+ is intended to work but has not
been verified; do not describe it as supported until a Windows 10 run passes the
full suite. Windows on ARM and non-Windows platforms are not supported.
