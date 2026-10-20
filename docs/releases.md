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
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 -Version v0.1.0

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

Existing sessions remain in `%LOCALAPPDATA%\FACT`. A replaced daemon is cold
recovered on the next launch; saved output is restored but old processes are
gone by design.

To install a local build:

```powershell
cargo build --release --bin fact
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 `
  -BinaryPath .\target\release\fact.exe -Force
```

Local builds do not have a release checksum. The script says so explicitly.

## Verify a download manually

```powershell
Get-FileHash .\fact-v0.1.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256
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
the explicit `-DataDir`. It refuses to purge a drive root or the user profile
root.

## Release process

1. Merge all changes to `main` through a pull request. CI must pass on the PR.
2. Run the local gates on Windows 11:

   ```powershell
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace --no-fail-fast
   ```

3. Complete the manual smoke checklist in
   [testing.md](testing.md#manual-smoke-check), including a real detach/reattach
   PID check and a cold recovery.
4. Ensure `Cargo.toml`'s workspace version matches the intended tag
   (`0.1.0` -> `v0.1.0`). Update `CHANGELOG.md`.
5. Create the annotated tag on the verified `main` commit and push it:

   ```powershell
   git switch main
   git pull --ff-only
   git tag -a v0.1.0 -m "FACT v0.1.0"
   git push origin v0.1.0
   ```

6. The `Release` workflow runs. Its `verify` job enforces:

   - the tag matches the workspace version,
   - the tagged commit is an ancestor of `origin/main`,
   - `cargo fmt --all -- --check`,
   - `cargo clippy --workspace --all-targets -- -D warnings`,
   - `cargo test --workspace --no-fail-fast`.

   Only then does the `package` job build `--release --locked`, smoke-test the
   binary, create the zip and `SHA256SUMS.txt`, upload the workflow artifact and
   publish the GitHub release.
7. Verify the published assets from a clean directory:

   ```powershell
   gh release view v0.1.0 --repo imthegoodboy/FACT
   gh release download v0.1.0 --repo imthegoodboy/FACT
   Get-FileHash .\fact-v0.1.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\SHA256SUMS.txt
   ```

8. Install the downloaded zip through `scripts/install.ps1` and run
   `fact doctor`, `fact --grid 2` and a detach/resume check.

Do not create a release manually. A release without the gate run is not a FACT
release.

## Artifact contents

```text
fact-v0.1.0-x86_64-pc-windows-msvc/
  fact.exe
  README.md
  LICENSE
  SECURITY.md
  CHANGELOG.md
  scripts/
    install.ps1
    uninstall.ps1
  docs/
    ...
```

`SHA256SUMS.txt` contains the SHA-256 of the zip.

## Versioning and rollback

- Use semantic versioning. A tag is `v` plus the workspace version.
- Tags are immutable. If an artifact is wrong, do not move the tag or force-push.
  Publish a new patch release with the fix.
- A bad release can be marked as a pre-release or removed from the releases page,
  but users who already downloaded it are not affected retroactively. Rotate any
  credentials that may have been exposed.
- Release notes are generated by GitHub from commits and pull requests; link the
  migration or breaking-change notes there when needed.

## Compatibility statement

v0.1.0 targets Windows 11 x64. Windows 10 1809+ is intended to work but has not
been verified; do not describe it as supported until a Windows 10 run passes the
full suite. Windows on ARM and non-Windows platforms are not supported.
