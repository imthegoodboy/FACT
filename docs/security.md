# Security model

FACT runs shells with your Windows user's full privileges. It does not sandbox
programs, intercept their network access or provide a multi-user server. Its
security goal is narrow: keep terminal control and scrollback inside the local
FACT process, avoid creating a remotely reachable service, and be clear about
the boundary that remains.

## Assets

- control of live ConPTY processes, including keyboard input and process exit
- terminal output and scrollback held in process memory, which may contain secrets
- configuration, including profile commands and environment values
- clipboard contents read for paste and written by copy

There is no session database, saved layout, export file, daemon identity or
authentication token to protect.

## Trust boundaries

```text
other Windows users / remote machines
        |
        | no listener, no IPC endpoint, no shared FACT state
        v
your Windows user  <-- security boundary
        |
        +-- any process running as you can inspect process memory and act as you
```

The same-user boundary is deliberate. FACT cannot defend against malware already
running as your account: such a process can read the FACT process's memory,
inject input into its console window or read the clipboard. Windows
administrators and SYSTEM can also inspect processes and bypass file ACLs. FACT
is not a security boundary against those principals.

## No local or remote service

FACT creates no TCP or UDP socket, HTTP server or named-pipe listener. It does
not install a service, request elevation or leave a process behind when the UI
closes. The only executable is `fact.exe`, and it exits with its shells.

This removes the authentication, ACL, pipe-squatting and stale-identity surface
that a client/server design would need. It also means there is no control
endpoint outside the process for another client to secure.

## Files and permissions

The only file FACT reads is an optional `config.toml`. The default location is
`%LOCALAPPDATA%\FACT\config.toml`, which Windows already scopes to the user
profile. `FACT_HOME` or `--config` can point elsewhere.

- A normal launch never writes a file.
- `fact config init` writes the configuration file requested by the user.
- No terminal output, input history, scrollback or layout is persisted.
- No log file is created. Errors are printed to the console.

Configuration is user-readable. Do not put secrets in `[env]` or in profile
arguments; values remain in the process environment and are visible to child
processes and same-user inspection.

## Profiles and child processes

A profile is trusted local configuration that names an executable and argument
list. FACT does not run profile arguments through `cmd.exe`, but the executable
can do anything your account can do. Do not use a configuration file from an
untrusted source, and review `[profiles]` commands before launching.

ConPTY child processes run at the user's integrity level. FACT does not elevate,
install a service or create a kernel driver. A per-pane job object ensures child
processes are cleaned up when FACT closes the PTY, subject to normal Windows
process semantics.

## Clipboard

Copy and paste use the normal Windows user clipboard:

- Selecting text only keeps a snapshot in FACT's process memory.
- A copy writes selected text to the clipboard, where other same-user processes
  can read it.
- A paste reads the current clipboard and sends it to the focused shell, which
  may execute it or forward it to a remote system.
- FACT does not inspect clipboard history or persist clipboard contents.

Use the standard Windows clipboard controls if you need to clear it after
handling sensitive data.

## Terminal output and memory

Commands, environment dumps, tokens printed to the terminal and prompts remain
in process memory and scrollback until overwritten or FACT exits. They are not
written to a database, log or export. Crash dumps or debugger memory captures may
still contain them; use BitLocker or another volume encryption product for
data-at-rest protection.

## Threats considered

| Threat | Control |
| --- | --- |
| Remote connection to FACT | No listener; FACT is a local console process. |
| Another local user controls FACT | No shared endpoint; Windows process and console boundaries apply. |
| Persisted terminal output leaks later | FACT writes no terminal output, history or layout. |
| Malicious profile command | Configuration is trusted input; review profiles before launching. |
| Clipboard leakage | Copy only when requested; clipboard is user-wide and not encrypted by FACT. |
| Orphaned shells after UI exit | Engine shutdown kills every PTY and its job object. |
| A full-screen app captures the mouse | Shift+drag selects; Shift+right-click opens FACT's menu. |

## Out of scope and limitations

- **Same-user compromise.** Malware, a debugger or any process running as your
  user can inspect FACT memory, read terminal output and control shells.
- **Administrators and SYSTEM.** They can inspect processes and bypass ACLs.
- **No encryption at rest.** Use BitLocker or another volume encryption product
  for data-at-rest protection.
- **No code signing yet.** Release artifacts are accompanied by SHA-256
  checksums. Verify them before running. SmartScreen reputation may be absent.
- **Terminal output can contain secrets.** It remains in process memory and
  scrollback until FACT exits.
- **Child processes are not sandboxed.** Shells and programs run with your normal
  user rights.
- **Windows 10 is not verified.** See the compatibility section in the README.
- **Window-level denial of service.** A same-user or administrator process can
  still close the console or exhaust resources.

## Incident response

If you suspect that terminal output exposed a credential, close FACT and rotate
that credential. Closing FACT terminates its shells; it does not alter files
outside the process. If you suspect a malicious configuration, review
`config.toml` before launching FACT again or start with `--config` pointing at a
known-good file.

To report a vulnerability, use GitHub's private security advisory flow for
`imthegoodboy/FACT` rather than a public issue with exploit details.
