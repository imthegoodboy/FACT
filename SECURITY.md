# Security policy

## Supported versions

FACT is pre-1.0. Security fixes target the latest release and `main`.

## Reporting a vulnerability

Please use GitHub's private security advisory flow:

<https://github.com/imthegoodboy/FACT/security/advisories/new>

Include the FACT version, Windows version, build source (release asset or commit)
and reproduction steps. Do not open a public issue with exploit details. Do not
include real credentials, private terminal output or other sensitive data.

## Security model

FACT is a single local process. It installs no service, requires no elevation,
opens no TCP/UDP socket, HTTP server or named-pipe listener, and leaves no
background process behind when the window closes. Shells and programs run with
your normal Windows user rights.

The full threat model, controls and limitations are in
[docs/security.md](docs/security.md). In particular:

- there is no IPC endpoint or authentication token; a process already running as
  your user can inspect FACT's memory and control its console;
- terminal output and scrollback exist only in process memory, with no database,
  log or export file;
- configuration is trusted local input, and profile commands run with your
  rights;
- the Windows clipboard is user-wide, so copied text leaves FACT and pasted text
  reaches the focused shell;
- release artifacts are checksummed but not code-signed.

## Process lifetime and data

Closing FACT, closing the last tab or choosing quit terminates every shell it
owns. There is no detach, resume, saved layout or crash recovery. Unsaved work in
a shell is lost when the process ends, and a crash dump or debugger capture may
still contain terminal output. Use BitLocker or another volume encryption product
for data-at-rest protection.

A normal launch reads an optional `config.toml` and writes nothing. `fact config
init` is the only command that creates or replaces that file. If you suspect a
credential was printed to a terminal, close FACT and rotate the credential; FACT
does not persist the output.
