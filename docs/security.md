# Security model

FACT runs shells with your Windows user's full privileges. It does not sandbox
programs, intercept their network access, or provide a multi-user server. Its
security goal is narrower: expose control of those shells only to the same
Windows user through local, authenticated IPC, and avoid creating a remotely
reachable service.

## Assets

- control of live ConPTY processes, including keyboard input and process exit
- terminal output and saved visual snapshots, which may contain secrets
- the session database, layout and export files
- the daemon identity file and authentication token
- configuration, including profile commands and environment values

## Trust boundaries

```text
other Windows users / remote machines
        |  no pipe access, no network listener
        v
your Windows user  <-- security boundary
        |
        +-- any process running as you can read your data and control FACT
```

The same-user boundary is deliberate. FACT cannot defend against malware already
running as your account: such a process can read `%LOCALAPPDATA%\FACT`, attach to
the daemon, inject keystrokes and read saved terminal output. Windows
administrators and SYSTEM can also bypass file and pipe ACLs. FACT is not a
security boundary against those principals.

## Local-only IPC

FACT uses Windows named pipes under `\\.\pipe\fact-...`. It creates no TCP or UDP
sockets and listens on no network interface. Pipe names are validated to be a
single local pipe component; remote forms such as `\\server\pipe\...` are
rejected.

The listening pipe uses:

- `PIPE_REJECT_REMOTE_CLIENTS`
- `FILE_FLAG_FIRST_PIPE_INSTANCE` for the first instance, preventing a squatter
  from owning the name before the daemon binds it
- byte mode with 64 KiB buffers
- a protected DACL generated for the current user SID and SYSTEM

The effective DACL is:

```text
D:P(A;;GA;;;<current-user-SID>)(A;;GA;;;SY)
```

SYSTEM is included so backup and security software can inspect the object. It is
not a second interactive user. The DACL is protected (`P`), so it does not
inherit broader access from a parent directory.

## Peer verification and handshake

After accepting a connection, the daemon calls
`GetNamedPipeClientProcessId`, opens that process with limited query rights, reads
its token user SID and compares it with the daemon's own user SID. A mismatch is
closed with `PermissionDenied` before the protocol starts.

Clients open named pipes with identification-only impersonation
(`SECURITY_IDENTIFICATION`). This ensures a malicious server cannot use the
client's token to impersonate it at a higher level merely because the client
connected.

Application authentication is still required:

1. The daemon generates a 256-bit token from the operating system CSPRNG.
2. It writes the token into `runtime\daemon.json`. The runtime directory, base
   directory and file are ACL-protected before token content is written. The file
   is published by writing a protected temporary file and renaming it.
3. The client reads and validates the identity document: protocol version, PID,
   pipe name shape and exactly 64 hex token characters.
4. Client and server exchange `Hello` messages. The daemon compares the token in
   constant time and rejects a mismatch as `Unauthorized`.
5. A successful attach returns a short-lived, single-use event-stream token.
   The event connection authenticates with that token, and the grant is consumed
   on subscription.

Request and response IDs are correlated, messages are length-prefixed and
bounded, and malformed bincode is rejected. A client cannot send commands on an
unauthenticated connection.

## Files and permissions

The data root is `%LOCALAPPDATA%\FACT` by default, which Windows already scopes to
the user profile. FACT additionally protects the base and runtime directories
with a DACL for the current user and SYSTEM. Snapshot, database and log files live
under that root.

The daemon identity file is not encrypted; its protection is the file ACL. On a
machine where another principal can take ownership or read raw disk content,
BitLocker or other volume encryption is the appropriate control.

## Snapshots, imports and execution

Cold recovery is data replay, not command replay:

- snapshots are ANSI visual content, not shell history or an input log
- a snapshot has a size limit, rejects NUL bytes, carries its dimensions and a
  SHA-256 checksum
- session load validates pane IDs, layout consistency, dimensions and checksums
- invalid data fails closed; FACT does not fall back to unvalidated raw input
- recovered output is written into a fresh terminal model with a fresh shell;
  saved bytes are never submitted to the shell as commands
- imports are parsed as versioned JSON, validated, and always receive a new
  session ID, fresh pane identity and reset PIDs/exit codes

Session exports contain terminal output and metadata. Treat them as sensitive
files and review them before sharing. Importing a session does not execute saved
content, but the new shells still inherit your profile command and environment.

## Profiles and child processes

A profile is trusted local configuration that names an executable and argument
list. FACT does not run profile arguments through `cmd.exe`, but the executable
can do anything your account can do. Do not use a configuration file from an
untrusted source, and review `[profiles]` commands before launching.

ConPTY child processes run at the user's integrity level. FACT does not elevate,
install a service or create a kernel driver. A per-pane job object ensures child
processes are cleaned up when FACT closes the PTY, subject to normal Windows
process semantics.

## Logging and error output

FACT logs lifecycle, errors, PIDs, sizes and recovery decisions. It does not log
the authentication token or configured environment values. Terminal content is
not copied into logs. Error messages can include file paths. The client does not
print the daemon token.

## Threats considered

| Threat | Control |
| --- | --- |
| Remote connection to the daemon | Local-only pipe name, `PIPE_REJECT_REMOTE_CLIENTS`, no network listener. |
| Another local user connects | Protected current-user/SYSTEM DACL and server-side peer SID check. |
| Another process squats the pipe name before daemon start | `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance. |
| Client impersonation by a malicious server | Identification-only impersonation. |
| Unauthenticated command injection over IPC | Random 256-bit token, constant-time comparison, one-time stream grants. |
| Forged or corrupted recovery data | Snapshot checksum, dimensions, size/NUL checks and layout validation. |
| Imported session overwrites an existing one | Import always creates a fresh identity; name collisions require explicit overwrite. |
| Remote terminal escape tricks through snapshots | Snapshots are validated visual ANSI and replayed into an isolated terminal model, never executed as shell input. |
| Credential leakage in logs | Token and environment values are not logged. |

## Out of scope and limitations

- **Same-user compromise.** Malware, a debugger or any process running as your
  user can attach to FACT, read terminal output and control shells.
- **Administrators and SYSTEM.** They can take ownership, read process memory or
  bypass ACLs.
- **No encryption at rest.** Use BitLocker or another volume encryption product
  for data-at-rest protection.
- **No code signing yet.** v0.1.0 release artifacts are accompanied by SHA-256
  checksums. Verify them before running. SmartScreen reputation may be absent.
- **Terminal output can contain secrets.** Commands, environment dumps, tokens
  printed to the terminal and prompts may be saved in snapshots. Clear sensitive
  output or reduce `snapshot_max_bytes`/scrollback if needed. Snapshots capture
  the visible screen, not unlimited history, but they are still sensitive.
- **Child processes are not sandboxed.** Shells and programs run with your normal
  user rights.
- **Windows 10 is not verified for v0.1.0.** See the compatibility section in the
  README.
- **Named-pipe availability.** A denial-of-service by a same-user or administrator
  process is not preventable by FACT.

## Token rotation and incident response

Restart the daemon to rotate the token:

```powershell
fact shutdown
fact resume
```

A graceful shutdown snapshots live sessions and removes the identity file only if
it still contains the daemon's token. A new daemon writes a new token. If you
suspect same-user compromise, stopping FACT is not sufficient: rotate any
credentials that appeared in terminal output and investigate the account.

To report a vulnerability, use GitHub's private security advisory flow for
`imthegoodboy/FACT` rather than a public issue with exploit details.
