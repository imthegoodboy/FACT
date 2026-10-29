# Install, uninstall and releases

FACT releases are per-user Windows x64 builds. They use no installer service,
MSI or administrator rights. FACT is one `fact.exe`: it installs no background
process, starts none and keeps no workspace on disk.

## Install a release

Download the zip and `SHA256SUMS.txt` from the
[releases page](https://github.com/imthegoodboy/FACT/releases), then:

```powershell
# Recommended: download and install the latest release through the script.
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1

# Pin a version (replace X.Y.Z with the release you want):
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 -Version vX.Y.Z

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
5. copies it to `%LOCALAPPDATA%\Programs\FACT`,
6. adds that directory to the **user** PATH unless `-NoPath` is given.

Close every running FACT window before replacing the executable. Windows locks a
running `fact.exe`, and FACT has no background process that could be stopped on
your behalf. The installer retries for a few seconds, then reports that the file
is in use.

An existing `config.toml` is kept, but it is read only. Every launch starts a
fresh workspace with fresh shells; no pane, tab, layout or scrollback is stored
between runs.

To install a local build:

```powershell
cargo build --release --bin fact
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 `
  -BinaryPath .\target\release\fact.exe -Force
```

Local builds do not have a release checksum. The script says so explicitly.

## Install with npm

```powershell
npm install -g fact-terminal
```

The `fact-terminal` package contains the same release `fact.exe`. npm's `fact`
command runs it directly; Node.js is not involved at runtime. If you also used
`install.ps1`, two `fact` commands exist on PATH and the first one wins;
`fact doctor` lists both.

Maintainers build the package from a release binary:

```powershell
cargo build --release --locked --bin fact
.\scripts\pack-npm.ps1                     # checks the version, writes dist\fact-terminal-X.Y.Z.tgz
npm publish .\dist\fact-terminal-X.Y.Z.tgz --access public
```

The release workflow does the same automatically when the repository has an
`NPM_TOKEN` secret, before it makes the GitHub release public. It skips a
version that is already on npm, so re-running a failed release job is safe.

## Verify a download manually

```powershell
Get-FileHash .\fact-vX.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256
Get-Content .\SHA256SUMS.txt
```

The zip's SHA-256 must match the line for that filename. The release workflow
generates both files in the same job, after the gates pass.

## Upgrading from a daemon-based build

The single-process build removes the daemon, named pipe, session database,
saved layouts and export/import commands. Commands such as `fact resume`,
`sessions`, `attach`, `detach`, `save`, `export`, `import` and `shutdown` no
longer exist. (`fact doctor` returned in 0.5.0 as a read-only system report.) `FACT_HOME` and `--config` still locate the optional
configuration file.

Configuration schema is version 2. Files containing removed keys such as
`max_sessions`, `persist_interval_secs` or `daemon_log_level` are rejected with
an unknown-field error. Create a clean template with `fact config init`, then
copy in only the profiles, environment and keybindings you still use.

## Uninstall

```powershell
# Remove the program and user PATH entry; keep configuration.
powershell -ExecutionPolicy Bypass -File .\scripts\uninstall.ps1

# Also remove %LOCALAPPDATA%\FACT (configuration). Irreversible.
powershell -ExecutionPolicy Bypass -File .\scripts\uninstall.ps1 -PurgeConfig

# Custom locations:
$env:FACT_HOME = 'D:\fact-config'
powershell -ExecutionPolicy Bypass -File .\scripts\uninstall.ps1 `
  -InstallDir C:\Tools\FACT -PurgeConfig
```

The uninstaller removes only `fact.exe` from the install directory, removes the
exact user PATH entry and keeps unrelated files. There is no service or daemon to
stop; close any running FACT window if you also want to delete its installed
executable immediately.

With `-PurgeConfig`, it removes `FACT_HOME` if set, otherwise
`%LOCALAPPDATA%\FACT`. It refuses to purge a drive root, the user profile root,
an ancestor of the profile, or a non-empty directory without a `config.toml`
marker. `-PurgeData` is accepted as a compatibility alias for `-PurgeConfig`.

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
   [testing.md](testing.md#manual-smoke-check), including the mouse, clipboard
   and shell-cleanup checks.
4. Ensure `Cargo.toml`'s workspace version matches the intended tag
   (`X.Y.Z` -> `vX.Y.Z`). Update `CHANGELOG.md`.
5. Create the annotated tag on the verified `main` commit and push it:

   ```powershell
   git switch main
   git pull --ff-only
   git tag -a vX.Y.Z -m "FACT vX.Y.Z"
   git push origin vX.Y.Z
   ```

6. The `Release` workflow runs. Its `verify` job enforces:

   - the tag matches the workspace version,
   - the tagged commit is an ancestor of `origin/main`,
   - `cargo fmt --all -- --check`,
   - `cargo clippy --workspace --all-targets -- -D warnings`,
   - `cargo test --workspace --no-fail-fast`.

   Only then does the `package` job build `--release --locked`, smoke-test the
   binary and configuration output, exercise the installer download path offline,
   create the zip and `SHA256SUMS.txt` and upload the workflow artifact. It
   creates a **draft** GitHub release, verifies that both expected assets are
   attached, and only then publishes the draft.
7. Verify the published assets from a clean directory:

   ```powershell
   gh release view vX.Y.Z --repo imthegoodboy/FACT
   gh release download vX.Y.Z --repo imthegoodboy/FACT
   Get-FileHash .\fact-vX.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\SHA256SUMS.txt
   ```

8. Extract the downloaded zip and run its `scripts\install.ps1`, not a script
   from a different checkout. Then verify the installed binary:

   ```powershell
   $exe = "$env:LOCALAPPDATA\Programs\FACT\fact.exe"
   & $exe --version
   & $exe config validate
   .\scripts\test-install.ps1 -FactExe $exe
   ```

   Finally run the manual checks in
   [testing.md](testing.md#manual-smoke-check) with the installed binary for the
   release sign-off.

Do not create a release manually. A release without the gate run is not a FACT
release.

## Artifact contents

```text
fact-vX.Y.Z-x86_64-pc-windows-msvc/
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
- Removing process, transport and persistence features is a breaking change;
  release it as a major version for 1.x or a clearly documented 0.x minor.
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

Current releases target Windows 11 x64. Windows 10 1809+ is intended to work but
has not been verified; do not describe it as supported until a Windows 10 run
passes the full suite. Windows on ARM and non-Windows platforms are not
supported.
