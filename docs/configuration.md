# Configuration

Deployment configuration (`install.conf`) is literal `KEY=VALUE` data read by the
installer. Runtime configuration (`.env`) is read by the Rust service. Neither is
executed as a shell script. The installer records selected deployment values so
that reconfiguration can regenerate Nginx/systemd consistently.

## Installer settings

| Setting | Default / meaning |
| --- | --- |
| `DOMAIN` | Required DNS hostname; no embedded scheme/path |
| `REPOSITORY` | `vast-development-method/jcb-api` |
| `INSTALL_DIR` | `/opt/jcb-api`, root-owned code and release symlink |
| `STATE_DIR` | `/var/lib/jcb-api`, private mutable state |
| `CONFIG_DIR` | `/etc/jcb-api`, root-owned configuration |
| `SERVICE_NAME` | `jcb-api` |
| `SERVICE_USER` | `jcb-api`, dedicated unprivileged Linux account |
| `NGINX_GROUP` | `www-data`, grants access only to the public socket |
| `PUBLIC_KEY_FILE` | Required trusted Ed25519 public key in SPKI PEM format |
| `SERVER_MEMORY_MB` | `512`, total memory allocation in MiB; minimum 256 |
| `SERVER_CPU_COUNT` | `2`, CPU allocation |
| `NETWORK_RATE` | `100`, sustained public requests per second per client IP |
| `NETWORK_BURST` | `500`, public burst capacity; minimum 100 |
| `WEBHOOK_RATE` | `10`, separate monitoring request rate per second |
| `WEBHOOK_BURST` | `50`, separate monitoring burst capacity |
| `ENABLE_HTTPS` | `true`, configure Let's Encrypt via Certbot |
| `ACME_EMAIL` | Required when HTTPS is enabled |
| `AUTO_UPDATE` | `true`, enable the stable-release update timer |
| `UPDATE_INTERVAL` | `15min`; positive duration ending in `s`, `min`, `h`, or `d` |
| `LEGACY_ENV_FILE` | Empty for new installations; existing `.env` path for migration |
| `RELEASE_VERSION` | `latest`, or an explicit stable `vX.Y.Z` release |
| `TRUSTED_PROXY_CIDRS` | Empty; comma-separated trusted proxy IPs/CIDRs |
| `LOG_NETWORK_REQUESTS` | `true`, detailed successful public-request events |
| `LOG_QUEUE_CAPACITY` | `8192`, bounded asynchronous logging queue |
| `MAX_CONTENT_LENGTH` | `1048576`, maximum monitoring request size in bytes |
| `MAX_STATE_BYTES` | Default: service memory allowance divided by 32; 8 MiB on a 512 MiB installation |
| `POLL_INTERVAL_SECONDS` | `5`, external state/token file reload check interval |

Paths must be absolute and contain only letters, digits, `_`, `.`, `-`, and `/`.
The installer rejects spaces, traversal, symlinks where a regular configuration
file is required, and nested/overlapping release, state and configuration
locations. Release/configuration ancestry must also be root-owned, non-symlink,
and not group/world writable; a writable home directory is not suitable for
trusted code or updater configuration. The application can use filesystem paths through runtime options;
these bootstrap restrictions make generated shell/systemd/Nginx configuration
unambiguous.

A proxy should be trusted only when it is controlled and actually forwards
traffic to this server. Never set a catch-all trust range just to enable forwarded
IP handling: arbitrary clients could then forge their identity and evade limits.

## Runtime settings

The generated `.env` selects the state paths, socket paths, limits, and service
identity. CLI commands accept `--env-file PATH`, which should precede the command.
Environment variables supplied by the process environment take precedence over
`.env` defaults.

| Setting | Purpose |
| --- | --- |
| `CACHE_FILE_PATH` | Existing compatible nested JSON network state |
| `TOKENS_FILE_PATH` | JSON object mapping system names to arrays of token strings |
| `API_SOCKET_PATH` | HTTP listener socket, normally `/run/jcb-api/api.sock` |
| `ADMIN_SOCKET_PATH` | Private administration socket, normally `/run/jcb-api/admin.sock` |
| `SERVER_MEMORY_MB` | Resource allocation used for bounded defaults |
| `SERVER_CPU_COUNT` | Runtime worker allocation |
| `MAX_STATE_BYTES` | Reject oversized state before publication |
| `MAX_CONTENT_LENGTH` | Request body size ceiling |
| `MAX_CONNECTIONS` | Maximum accepted public socket connections; allocation-derived default |
| `LOG_NETWORK_REQUESTS` | Enable detailed network-call events |
| `LOG_QUEUE_CAPACITY` | Logging queue capacity |
| `POLL_INTERVAL_SECONDS` | External file refresh interval |
| `PUBLIC_BASE_URL` | Canonical external base URL for deployment-related responses |
| `SERVICE_NAME` | Service used by the log-following command |

Cache updates use unique temporary files beside the destination, followed by an
atomic replacement. The old `TEMP_CACHE_FILE_PATH` setting can remain in migrated
configuration but does not select a shared temporary file in the Rust engine.
The old background debounce is replaced by persistence before acknowledgement.

Old `LOG_FILE_PATH`, `LOG_FILE_MAX_SIZE`, and `LOG_FILE_BACKUP_COUNT` values describe
the previous application's logs. They do not control the new systemd journal.
Keep the old logs for history; use journald retention settings for new activity.

## Resource sizing

For a 512 MiB allocation, the installer sets the service memory maximum to
256 MiB and a pressure threshold below that. The remainder is available to the
kernel, Nginx and other necessary system services. The default maximum state file
size is 8 MiB at this allocation, leaving room for prepared responses and other
working memory. Increase the allocation and
reconfigure when moving to a larger server; do not set it to the host's entire
physical memory when other workloads need a share.

The engine also bounds individual state files, request bodies and work queues.
The systemd ceiling is a final containment boundary: reaching it can terminate a
process. It is not a substitute for keeping normal working state comfortably
below the allowance. Configuration changes do not execute benchmarks and are not
recalculated on each API request.

To change deployment limits, edit the saved installer configuration and rerun the
installer. To change a runtime-only setting directly, edit `.env` and restart the
service; keep that override documented because a future installer reconfiguration
regenerates the environment file.
