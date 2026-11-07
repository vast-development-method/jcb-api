# JCB API

A native Rust availability directory for Joomla Component Builder. Public network
requests return prepared JSON from memory. Authenticated monitoring updates grow
the directory dynamically. A local CLI manages records and tokens through a
private Unix socket.

The service preserves the existing network and monitoring URLs and JSON response
shapes. It runs as an unprivileged systemd service behind Nginx. Releases contain
prebuilt Linux binaries; production servers do not need Rust, Python, Gunicorn,
Redis, or a database. The downloaded installer is a small Bash bootstrap; the
application, administration client, state engine, and updater are Rust.

## Public API

| Method | Path | Response |
| --- | --- | --- |
| GET | `/v1/network/` | All systems: `{"systems": [...]}` |
| GET | `/v1/network/{system}/` | A system and its projects |
| GET | `/v1/network/{system}/{project}/` | A project and its targets |
| GET | `/v1/network/{system}/{project}/{target}` | A target and its network nodes |
| GET | `/v1/network/{system}/{project}/{target}/{status}` | A target with nodes matching the integer status |

`1` denotes online and `0` offline. Other integer status filters are accepted; an
unmatched value returns an empty `network` array. Names are discovered from the
parent resource and can grow without changing routes or restarting. System and
project paths use trailing slashes; target and status paths do not. `HEAD` and
`OPTIONS` are supported, with public CORS access. Missing resources retain the
legacy JSON errors and HTTP 403; an invalid status filter returns HTTP 400.

The supplied HTML design is served at `/`. `/openapi.json` and `/OpenAPI.json`
return the same OpenAPI 3.1 document. These public documents describe only the
network API. They contain no monitoring or administrative routes. Examples in
the HTML adapt to the current domain. The specification uses the configured public base URL, or a relative server URL
when none is configured, so a new domain does not require editing code.

## Before installation

1. Use a supported Debian or Ubuntu server with systemd and root/sudo access.
2. Point the domain's DNS records to the server. Open TCP 80 and 443 in the
   firewall and upstream security group if the installer is to configure HTTPS.
   If publishing an AAAA record, ensure IPv6 reaches the same server.
3. Obtain the release **public key PEM** from the maintainer through a trusted
   channel and save it locally. Verify its fingerprint through that channel.
   Do not trust a key merely because it accompanies the downloaded binary.
4. Ensure at least one signed, stable GitHub Release exists. Maintainers should
   complete [release setup](docs/releasing.md) before installing the first server.
5. For migration, retain the old `.env`, cache, token file, and logs. The installer
   can import the cache and tokens; it does not delete or overwrite those sources.

Public releases are downloadable without a GitHub token. The server never needs
repository write access or release signing credentials.

## Install

Download and inspect the installer, then run it:

```bash
curl --fail --silent --show-error --location \
  https://raw.githubusercontent.com/vast-development-method/jcb-api/main/scripts/install.sh \
  --output jcb-api-install.sh
less jcb-api-install.sh
sudo bash ./jcb-api-install.sh
```

It asks for the domain, installation/state/configuration directories, service
name and account, trusted public key file, CPU/memory allocation, optional legacy
`.env`, HTTPS email, and whether to enable automatic updates. Press Enter to
accept an offered default. Supply an actual domain and public-key path when
asked; those intentionally have no production default.

The installer verifies the release manifest's Ed25519 signature and the binary's
SHA-256 digest **before executing it**, checks the reported version, creates the
service account, and configures systemd and Nginx. HTTPS uses Let's Encrypt via
Certbot; the renewal hook reloads Nginx. The first service activation gets a small
local health check. A previous binary is restored when activation fails.

The default layout is:

| Location | Owner / purpose |
| --- | --- |
| `/opt/jcb-api/releases/vX.Y.Z/jcb-api` | Root-owned verified release binaries |
| `/opt/jcb-api/current` | Atomic symlink to the active release |
| `/etc/jcb-api/.env` | Runtime configuration; no embedded token secrets |
| `/etc/jcb-api/install.conf` | Root-only saved deployment settings |
| `/etc/jcb-api/release-public.pem` | Trusted update verification key |
| `/var/lib/jcb-api/cache.json` | Durable network state |
| `/var/lib/jcb-api/tokens.json` | Private token state |
| `/run/jcb-api/api.sock` | Public HTTP socket; Nginx group access |
| `/run/jcb-api/admin.sock` | Private local administration socket |

These defaults can all be adapted through the installer and runtime settings.
Root-owned code and configuration are separate from service-owned state. The
service cannot replace its executable. Nginx has access to the HTTP socket but
not the private administration socket or token file.

### Migration from the Python service

Schedule a brief cutover so the old service does not keep accepting changes after
its files are copied. Stop the old service after it has persisted its latest
updates, then point the installer at the existing `.env`. The original script's
shutdown does not reliably flush pending writes, so allow its debounce interval
to complete and verify the cache reflects the last status before stopping it.

For the supplied installation, the installer imports the files named by
`CACHE_FILE_PATH` and `TOKENS_FILE_PATH`, including the existing tokens. It does
not import `cache_temp.json`, which is a temporary write artifact. Original log
files and their rotated backups stay untouched. Existing destination cache/token
files are preserved rather than overwritten. Back up these files before cutover.

If an old Nginx virtual host already owns the same domain, save it and disable its
symlink in `sites-enabled` before enabling the new host. Keep it available for
rollback. Check `sudo nginx -t` and ensure only one virtual host owns the domain.
The installer does not delete unrelated hosts or the legacy application's files.

### Unattended installation and reconfiguration

`--config FILE --non-interactive` reads literal `KEY=VALUE` settings. The file is
parsed as data, never sourced as shell code. Unknown keys and unsafe paths fail
validation. See [deployment settings](docs/configuration.md).

After an installation, edit the saved settings and re-run:

```bash
sudoedit /etc/jcb-api/install.conf
sudo bash ./jcb-api-install.sh \
  --config /etc/jcb-api/install.conf --non-interactive
```

Use the chosen configuration path when installing outside the default layout.
Reconfiguration preserves cache and tokens, saves the previous runtime `.env` as
`.env.before-install`, and regenerates service/Nginx settings. Keep custom runtime
options documented in the saved installer settings; a regenerated environment
replaces manual edits. The live updater only replaces binaries and does not
regenerate configuration.

To inspect generated files without root, downloads, or service changes:

```bash
bash ./jcb-api-install.sh --config ./install.conf \
  --non-interactive --render-only ./rendered-deployment
```

`install.conf` must contain the intended deployment values and a real trusted
public-key file. Render mode still validates the configuration.

## Local administration

The examples below use the default paths. Commands talk to the running service's
private socket, so mutations share the same ordering and persistence rules as
monitoring updates. Run them as root or the service account, not as the Nginx user.

```bash
# Inspect all stored network records.
sudo /opt/jcb-api/current/jcb-api --env-file /etc/jcb-api/.env network list

# Generate a new token for a monitoring system; record the displayed secret.
sudo /opt/jcb-api/current/jcb-api --env-file /etc/jcb-api/.env \
  token create --system community

# List fingerprints without revealing token values.
sudo /opt/jcb-api/current/jcb-api --env-file /etc/jcb-api/.env token list

# Remove one record, identified by its JSON ID.
sudo /opt/jcb-api/current/jcb-api --env-file /etc/jcb-api/.env \
  network remove --system community --project jcb --target super-powers --id 28 --yes

# Remove an entire target. Empty parent groups are cleaned up.
sudo /opt/jcb-api/current/jcb-api --env-file /etc/jcb-api/.env \
  network remove --system community --project jcb --target retired-target --yes

# Validate and reload externally edited files immediately.
sudo /opt/jcb-api/current/jcb-api --env-file /etc/jcb-api/.env reload
```

A string ID must be passed as a JSON string, for example `--id '"mirror-a"'`.
Deletion without `--id` applies to the explicitly selected target, project, or
system. `--yes` acknowledges the destructive scope. A future valid monitoring
update can recreate a deleted record; stop its sender to keep it absent.

For token rotation, create a new token, change the sender, verify a successful
update, then revoke the old fingerprint. The revoke command accepts
`token revoke --system SYSTEM --fingerprint FINGERPRINT`; obtain the exact
fingerprint with `token list` rather than entering secrets into shell history.
The token shown on creation must be stored securely. Tokens never appear in
ordinary listings or operational logs.

## Internal monitoring integration

Monitoring senders continue using:

```text
POST /v1/webhook/{system}/{project}/{target}
Authorization: the exact token value
Content-Type: application/json
```

The existing scheme is the raw `Authorization` header value. Do not add a
`Bearer ` prefix to an existing token unless that prefix is part of the stored
value. Tokens are authorized for their associated system.

A normal payload is:

```json
{
  "heartbeat": {"monitorID": 28, "status": 1, "msg": "Available"},
  "monitor": {
    "name": "codeberg.org/joomla/super-powers",
    "url": "https://codeberg.org/joomla/super-powers"
  }
}
```

The key is `(system, project, target, monitorID)`. Unknown groups are created
naturally; an existing record is updated. IDs may be integers or strings. A
successful change is persisted before its prepared public snapshot is published
and success is acknowledged. A failed save does not silently acknowledge a lost
change. Public documentation intentionally omits this operational integration;
token validation, not endpoint obscurity, provides its access control.

## Logs and service checks

```bash
sudo systemctl status jcb-api.service
sudo journalctl -u jcb-api.service -f
sudo journalctl -u jcb-api-update.service --since today
sudo systemctl list-timers jcb-api-update.timer
sudo /opt/jcb-api/current/jcb-api --env-file /etc/jcb-api/.env logs --follow
sudo curl --fail --unix-socket /run/jcb-api/api.sock http://localhost/healthz
```

The local health response reports status and binary version. Nginx does not expose
`/healthz` publicly. The service's journal includes timestamps, severity, network
request fields, monitoring event details, administrative changes, and persistence
errors. Successful network request logging is enabled by default. Token values
are never included. Logging uses bounded asynchronous buffering and reports
queue overflow; full request logging has an unavoidable output cost.

Journald controls retention and disk usage. Inspect `journalctl --disk-usage` and
configure journald limits appropriate to the whole host; do not assume the
application's old `LOG_FILE_MAX_SIZE` setting controls the system journal. The
installer disables journald's per-service rate suppression so that buffering
losses are observable rather than silently hidden by an additional rate limit.
Original file logs remain available separately for historical inspection.

## Automatic updates and rollback

The optional `jcb-api-update.timer` checks stable GitHub Releases at the configured
interval (15 minutes by default, with jitter). The separate root updater verifies
the signed manifest and digest, refuses downgrades, stages the correct native
binary, switches the release symlink, restarts the service, and checks the running
version through the local socket. Failed activation restores the previous
release. A failed verified release directory is retained for inspection.

```bash
sudo /opt/jcb-api/current/jcb-api update --check
sudo systemctl start jcb-api-update.service
sudo systemctl stop jcb-api-update.timer
```

Use the generated update service's arguments for non-default paths. Keep a backup
of cache and token files in addition to retained binaries. The updater never
rewrites these data files to an older schema. For explicit rollback, disable the
update timer, stop the service, atomically point `current` at an inspected retained
release, and restart it. Check compatibility of that binary with the current
state before doing so. See [release operations](docs/releasing.md).

## Resource limits and abuse protection

The installer accepts `SERVER_MEMORY_MB` and `SERVER_CPU_COUNT` as the allocation
for this installation, including virtual machines. It reserves half of the
memory for the operating system/Nginx and applies the other half as the service's
systemd memory ceiling, with a lower pressure threshold. Prepared state, request
sizes, logging queues and runtime concurrency are bounded. These are limits, not
a promise of fixed memory consumption or a throughput measurement.

Public traffic defaults to 100 requests/second per client IP with 500 requests of
burst capacity and no deliberate delay for allowed requests. An isolated burst
of around 100 JCB compilation calls fits. Multiple installations behind one NAT
share an allowance, so adjust it for your environment. Monitoring routes have
separate limits and Nginx rejects an empty/missing authorization header before
forwarding. A fabricated nonempty token still requires Rust validation.

Only explicitly configured proxy CIDRs may supply trusted forwarded client IPs.
Rate-limited requests return HTTP 429 and `Retry-After`. Resource caps and Nginx
limits protect the application; traffic that saturates an upstream link must be
handled upstream. No benchmarks or test suites run during normal service startup.

## Development and verification

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
bash scripts/ci-smoke.sh target/release/jcb-api
bash scripts/test-install.sh
```

CI verifies the public API, dynamic growth, durable mutations, token lifecycle,
local administration, documentation, migration behavior, installer rendering,
and signature verification. Release builds cover x86-64 and ARM64 Linux with
musl. CI uses temporary keys for signing tests; production keys are never used in
pull-request jobs. See [maintainer release setup](docs/releasing.md) for versioned
publishing and backfill builds. The repository's [LICENSE](LICENSE) applies.
