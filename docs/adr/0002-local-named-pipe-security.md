# 0002: Raw Windows named pipes with DACL and token authentication

- **Status:** Accepted
- **Date:** 2026-10-19

## Context

The client and daemon exchange commands and high-volume terminal frames. They
must not expose a network listener, and other local users must not control the
daemon. Options included TCP on loopback, gRPC/named pipes through a framework,
or direct Win32 named pipes.

TCP loopback still creates a network listener, can be reached through local
proxies and requires a separate authentication design. A generic IPC framework
adds dependencies and hides the Windows security details FACT needs to audit.
The project already depends on `windows-sys` for ConPTY.

## Decision

Use raw Win32 byte-mode named pipes with:

- local `\\.\pipe\fact-<hash-of-data-dir>` names
- `PIPE_REJECT_REMOTE_CLIENTS`
- `FILE_FLAG_FIRST_PIPE_INSTANCE`
- a protected DACL granting full access only to the current user and SYSTEM
- a server-side peer user SID check after `GetNamedPipeClientProcessId`
- identification-only impersonation on the client
- a 256-bit random daemon token in an ACL-protected `daemon.json`
- token authentication in a `Hello` exchange before any command
- short-lived, single-use tokens for event-stream subscriptions
- length-prefixed bincode messages with explicit size limits

A control connection carries requests and responses. A separate event-stream
connection carries frames, so a large frame burst cannot block command replies
or be mistaken for one.

## Consequences

- No network listener exists.
- Security behavior is inspectable and covered by Windows-specific tests.
- The wire protocol is FACT-specific and must be versioned and bounded.
- Named pipes are local to a machine but are a global namespace; pipe names must
  be validated and squatting prevented.
- SYSTEM is granted access for backup/security tooling; administrators and SYSTEM
  remain outside the threat boundary.
- Same-user processes can read the token by definition; the token is not a
  defense against same-user malware.

## Alternatives considered

- **Loopback TCP:** rejected for the no-listener requirement and exposure to
  local proxies/firewall rules.
- **Anonymous pipes:** cannot be discovered by an unrelated client process.
- **ALPC:** powerful but more complex and less portable across supported Windows
  versions.
- **Windows AppContainer/isolated user:** would complicate shell access and is
  not compatible with the goal of using the user's normal shells.
