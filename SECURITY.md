# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub Security Advisories:

**https://github.com/alforria-ai/alforria/security/advisories/new**

Do not open a public issue, discussion, or pull request for a vulnerability.
Include:

- the affected version (`alforria --version`);
- the operating system and install method;
- a description of the issue and its impact;
- reproduction steps or a proof of concept;
- any relevant logs, with secrets and transcript contents removed.

You will get an acknowledgement through the advisory thread. Please give the
maintainers time to investigate and ship a fix before public disclosure.

## Supported versions

alforria is pre-1.0. Security fixes land on the latest release and `main`.
Older releases are not maintained unless a maintainer says otherwise.

## Security model

alforria is a local coding agent. It reads and writes files, runs shell
commands, starts processes, and talks to model providers. Treat it with the
same care as any other tool that can execute code on your machine.

### Permissions

- Tools can be allowed, asked, or denied through the `permission` config.
  Rules can match tool names and, for shell commands, command prefixes.
- `--auto` auto-approves every permission that is not explicitly denied. It is
  dangerous and should only be used in an environment you are prepared to
  lose.
- Repeated identical tool calls can trigger the `doom_loop` permission gate.
- Review repository-level config and agent files before running alforria in an
  untrusted checkout. Project config can change allowed tools and permissions.

### Local server

- `alforria serve` binds to `127.0.0.1` by default.
- With `OPENCODE_SERVER_PASSWORD` unset, the CLI generates a random password,
  prints the username and password once, and requires HTTP Basic auth.
- Setting `OPENCODE_SERVER_PASSWORD` to an empty string is an explicit opt-out:
  the server prints a warning and runs unauthenticated. Do not do this on a
  shared or public network.
- The server rejects foreign `Origin` headers, permits its own origin and
  loopback names, and accepts extra origins only through `--cors`.
- PTY websockets refuse foreign origins.
- alforria does not terminate TLS. If the server is reachable beyond a trusted
  local network, put it behind a TLS-terminating reverse proxy or use an SSH
  tunnel. Do not expose a plain HTTP server to the internet.

### Secrets

- Provider credentials are stored in the alforria auth store and can be
  supplied through `{env:VAR}` or `{file:path}` substitutions in config.
- Do not commit credentials to `opencode.json`, project files, or
  `.opencode/`. The repository ignore rules cover common local state, not
  every possible secret.
- `alforria export --sanitize` redacts sensitive transcript and file data.
  Plain `alforria export` does not sanitize anything.

### Plugins and MCP servers

Plugins and MCP servers run with the same local privileges as alforria. Only
install plugins and servers you trust, and pin versions where possible. Use
`--pure` to run without external plugins.

### Snapshots and revert

Git-backed snapshots reduce the risk of losing work, but they are not a
sandbox or a backup system. Snapshot and revert require a Git repository, and
revert operates on the files listed in the snapshot. Keep your own backups.

## Out of scope

- Vulnerabilities in model providers or third-party services.
- Issues that require an already-compromised machine or account.
- Unauthenticated servers that were deliberately exposed to an untrusted
  network.
- Findings that depend on `--auto` or an explicit insecure configuration,
  unless the configuration bypasses a documented safety check.
