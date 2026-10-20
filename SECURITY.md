# Security policy

## Supported versions

FACT is pre-1.0. Security fixes target the latest `0.3.x` release and `main`.

## Reporting a vulnerability

Please use GitHub's private security advisory flow:

<https://github.com/imthegoodboy/FACT/security/advisories/new>

Include the FACT version, Windows version, build source (release asset or commit)
and reproduction steps. Do not open a public issue with exploit details. Do not
include real credentials, private terminal output or other sensitive data.

## Security model

FACT's IPC is local-only and same-user. It uses a protected named-pipe DACL, a
server-side peer user check, identification-only impersonation and a random
daemon token. It does not install a service, require elevation or listen on a
network interface.

The full threat model, controls and limitations are in
[docs/security.md](docs/security.md). In particular:

- a process already running as your user can control FACT and read its data;
- administrators and SYSTEM can bypass file and pipe ACLs;
- terminal output may contain secrets but is kept in daemon memory, not SQLite;
- legacy v0.2 databases are migrated to clear old snapshot columns;
- release artifacts are checksummed but not code-signed in v0.3.0.

## Token rotation

Restart the daemon to generate a new token:

```powershell
fact shutdown
fact resume
```

A graceful shutdown saves live layouts before removing the identity file.
