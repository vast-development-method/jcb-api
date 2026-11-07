#!/usr/bin/env bash
# Guided, repeatable installation of signed JCB API releases on Debian/Ubuntu.
# This file is deliberately self-contained so it can be downloaded on its own.
set -Eeuo pipefail
umask 077

die() { printf 'Error: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || die "Required command is missing: $1"; }

CONFIG_FILE=''
RENDER_DIR=''
NON_INTERACTIVE=false
while (($#)); do
    case "$1" in
        --config) (($# >= 2)) || die '--config needs a file'; CONFIG_FILE=$2; shift 2 ;;
        --render-only) (($# >= 2)) || die '--render-only needs a directory'; RENDER_DIR=$2; shift 2 ;;
        --non-interactive) NON_INTERACTIVE=true; shift ;;
        --help|-h)
            printf '%s\n' 'Usage: install.sh [--config FILE] [--non-interactive] [--render-only DIR]' \
                'Configuration is KEY=VALUE data, never executable shell.' \
                'Render-only validates configuration and writes examples without root, downloads, or system changes.'
            exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

REPOSITORY=vast-development-method/jcb-api
DOMAIN=''
INSTALL_DIR=/opt/jcb-api
STATE_DIR=/var/lib/jcb-api
CONFIG_DIR=/etc/jcb-api
SERVICE_NAME=jcb-api
SERVICE_USER=jcb-api
NGINX_GROUP=www-data
PUBLIC_KEY_FILE=''
SERVER_MEMORY_MB=512
SERVER_CPU_COUNT=2
NETWORK_RATE=100
NETWORK_BURST=500
WEBHOOK_RATE=10
WEBHOOK_BURST=50
AUTO_UPDATE=true
UPDATE_INTERVAL=15min
ENABLE_HTTPS=true
ACME_EMAIL=''
LEGACY_ENV_FILE=''
RELEASE_VERSION=latest
TRUSTED_PROXY_CIDRS=''
LOG_NETWORK_REQUESTS=true
LOG_QUEUE_CAPACITY=8192
MAX_CONTENT_LENGTH=1048576
MAX_STATE_BYTES=''
POLL_INTERVAL_SECONDS=5

# Parse a deliberately small data format. Quotes and inline comments are accepted;
# expansions, command substitutions, executable lines and unknown keys are not.
trim() { local text=$1; text="${text#"${text%%[![:space:]]*}"}"; text="${text%"${text##*[![:space:]]}"}"; REPLY=$text; }
parse_value() {
    local value=$1
    trim "$value"; value=$REPLY
    if [[ $value == \"* ]]; then
        [[ $value =~ ^\"([^\"]*)\"[[:space:]]*(#.*)?$ ]] || die 'Invalid quoted configuration value'
        value=${BASH_REMATCH[1]}
    elif [[ $value == \'* ]]; then
        [[ $value =~ ^\'([^\']*)\'[[:space:]]*(#.*)?$ ]] || die 'Invalid quoted configuration value'
        value=${BASH_REMATCH[1]}
    else
        value=${value%%[[:space:]]#*}
        trim "$value"; value=$REPLY
    fi
    [[ $value != *'$'* && $value != *'`'* && $value != *$'\r'* && $value != *$'\n'* ]] || die 'Configuration must contain literal values, not shell expressions'
    REPLY=$value
}
read_config() {
    local file=$1 line key value
    [[ -f $file && ! -L $file ]] || die "Configuration must be a regular file: $file"
    while IFS= read -r line || [[ -n $line ]]; do
        trim "$line"; line=$REPLY
        [[ -z $line || $line == \#* ]] && continue
        [[ $line =~ ^([A-Z][A-Z0-9_]*)[[:space:]]*=(.*)$ ]] || die "Invalid configuration line in $file"
        key=${BASH_REMATCH[1]}; value=${BASH_REMATCH[2]}
        case "$key" in
            REPOSITORY|DOMAIN|INSTALL_DIR|STATE_DIR|CONFIG_DIR|SERVICE_NAME|SERVICE_USER|NGINX_GROUP|PUBLIC_KEY_FILE|SERVER_MEMORY_MB|SERVER_CPU_COUNT|NETWORK_RATE|NETWORK_BURST|WEBHOOK_RATE|WEBHOOK_BURST|AUTO_UPDATE|UPDATE_INTERVAL|ENABLE_HTTPS|ACME_EMAIL|LEGACY_ENV_FILE|RELEASE_VERSION|TRUSTED_PROXY_CIDRS|LOG_NETWORK_REQUESTS|LOG_QUEUE_CAPACITY|MAX_CONTENT_LENGTH|MAX_STATE_BYTES|POLL_INTERVAL_SECONDS) ;;
            *) die "Unknown configuration key: $key" ;;
        esac
        parse_value "$value"
        printf -v "$key" '%s' "$REPLY"
    done < "$file"
}
[[ -z $CONFIG_FILE ]] || read_config "$CONFIG_FILE"

ask() {
    local key=$1 question=$2 value
    $NON_INTERACTIVE && return
    [[ -t 0 ]] || die 'Use --non-interactive with --config when stdin is not a terminal'
    read -r -p "$question [${!key}]: " value
    [[ -z $value ]] || printf -v "$key" '%s' "$value"
}
ask DOMAIN 'Public domain (DNS must point to this server)'
ask INSTALL_DIR 'Root-owned application release directory'
ask STATE_DIR 'Persistent application data directory'
ask CONFIG_DIR 'Configuration directory'
ask SERVICE_NAME 'Systemd service name'
ask SERVICE_USER 'Dedicated Linux account'
ask PUBLIC_KEY_FILE 'Trusted release public key PEM file'
ask SERVER_MEMORY_MB 'Total memory allocated to this installation in MiB'
ask SERVER_CPU_COUNT 'CPU allocation'
ask LEGACY_ENV_FILE 'Existing .env to migrate; leave empty for a fresh installation'
ask ENABLE_HTTPS "Configure HTTPS with Let's Encrypt (true/false)"
[[ $ENABLE_HTTPS != true ]] || ask ACME_EMAIL 'Email for certificate renewal notices'
ask AUTO_UPDATE 'Enable automatic stable-release updates (true/false)'

valid_path() {
    local path=$1
    [[ $path =~ ^/[A-Za-z0-9_./-]+$ && $path != / && $path != */../* && $path != */.. && $path != */./* && $path != */. && $path != *//* && $path != */ ]] || die "Use an absolute path without spaces, parent traversal or shell characters: $path"
}
number() { [[ ${!1} =~ ^(0|[1-9][0-9]{0,9})$ ]] || die "$1 must be a decimal integer without leading zeros"; ((${!1} >= $2 && ${!1} <= $3)) || die "$1 must be between $2 and $3"; }
bool() { [[ ${!1} == true || ${!1} == false ]] || die "$1 must be true or false"; }
[[ $DOMAIN =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ && $DOMAIN == *.* && $DOMAIN != *..* && ${#DOMAIN} -le 253 ]] || die 'DOMAIN must be a DNS hostname'
[[ $REPOSITORY =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die 'REPOSITORY must be owner/name'
[[ $SERVICE_NAME =~ ^[a-z][a-z0-9-]{0,30}$ && $SERVICE_USER =~ ^[a-z][a-z0-9-]{0,30}$ && $NGINX_GROUP =~ ^[a-z][a-z0-9-]{0,30}$ ]] || die 'Invalid service or account name'
[[ $SERVICE_USER != root && $SERVICE_USER != nobody && $SERVICE_USER != "$NGINX_GROUP" ]] || die 'SERVICE_USER must be a dedicated unprivileged application account'
for path in "$INSTALL_DIR" "$STATE_DIR" "$CONFIG_DIR"; do
    valid_path "$path"
    case "$path" in /etc|/usr|/usr/local|/bin|/sbin|/usr/bin|/usr/sbin|/var|/var/lib|/opt|/home|/root|/run|/tmp) die "Choose a dedicated application directory, not $path" ;; esac
done
[[ $INSTALL_DIR != "$STATE_DIR" && $INSTALL_DIR != "$CONFIG_DIR" && $STATE_DIR != "$CONFIG_DIR" ]] || die 'Release, configuration and state directories must be separate'
for parent in "$INSTALL_DIR" "$STATE_DIR" "$CONFIG_DIR"; do
    for child in "$INSTALL_DIR" "$STATE_DIR" "$CONFIG_DIR"; do
        [[ $parent == "$child" || $child != "$parent/"* ]] || die 'Release, configuration and state directories must not contain one another'
    done
done
[[ -z $LEGACY_ENV_FILE ]] || valid_path "$LEGACY_ENV_FILE"
[[ -n $PUBLIC_KEY_FILE ]] || die 'PUBLIC_KEY_FILE must name a trusted Ed25519 public key obtained from the maintainer'
valid_path "$PUBLIC_KEY_FILE"
[[ -f $PUBLIC_KEY_FILE && ! -L $PUBLIC_KEY_FILE ]] || die 'PUBLIC_KEY_FILE is not a regular file'
number SERVER_MEMORY_MB 256 1048576
number SERVER_CPU_COUNT 1 256
number NETWORK_RATE 1 1000000
number NETWORK_BURST 100 1000000
number WEBHOOK_RATE 1 1000000
number WEBHOOK_BURST 1 1000000
number LOG_QUEUE_CAPACITY 64 1048576
number MAX_CONTENT_LENGTH 1024 16777216
number POLL_INTERVAL_SECONDS 1 3600
for key in AUTO_UPDATE ENABLE_HTTPS LOG_NETWORK_REQUESTS; do bool "$key"; done
[[ $UPDATE_INTERVAL =~ ^[1-9][0-9]*(s|min|h|d)$ ]] || die 'UPDATE_INTERVAL must be a systemd duration such as 15min or 1h'
[[ $RELEASE_VERSION == latest || $RELEASE_VERSION =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die 'RELEASE_VERSION must be latest or a stable vX.Y.Z tag'
if $ENABLE_HTTPS; then
    [[ $ACME_EMAIL =~ ^[A-Za-z0-9_.+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$ ]] || die 'A valid ACME_EMAIL is required for HTTPS'
fi
SERVICE_MEMORY_MB=$((SERVER_MEMORY_MB / 2))
MEMORY_HIGH_MB=$((SERVICE_MEMORY_MB * 4 / 5))
[[ -n $MAX_STATE_BYTES ]] || MAX_STATE_BYTES=$((SERVICE_MEMORY_MB * 1024 * 1024 / 32))
number MAX_STATE_BYTES 4096 1073741824
((MAX_STATE_BYTES <= SERVICE_MEMORY_MB * 1024 * 1024 / 4)) || die 'MAX_STATE_BYTES must leave memory for prepared responses, requests and logging'
RUNTIME_DIR="/run/$SERVICE_NAME"
API_SOCKET_PATH="$RUNTIME_DIR/api.sock"
ADMIN_SOCKET_PATH="$RUNTIME_DIR/admin.sock"
TASKS_MAX=$((SERVER_CPU_COUNT * 32 + 64))
((TASKS_MAX <= 512)) || TASKS_MAX=512

PROXY_CONFIG=''
if [[ -n $TRUSTED_PROXY_CIDRS ]]; then
    IFS=',' read -r -a proxies <<< "$TRUSTED_PROXY_CIDRS"
    for proxy in "${proxies[@]}"; do
        [[ $proxy =~ ^[0-9a-fA-F:.]+(/[0-9]{1,3})?$ ]] || die 'Trusted proxies must be comma-separated IP addresses or CIDRs'
        PROXY_CONFIG+="    set_real_ip_from $proxy;"$'\n'
    done
    PROXY_CONFIG+=$'    real_ip_header X-Forwarded-For;\n    real_ip_recursive on;\n'
fi

TEMP_DIR=$(mktemp -d)
trap 'rm -rf -- "$TEMP_DIR"' EXIT
if [[ -n $RENDER_DIR ]]; then
    mkdir -p -- "$RENDER_DIR"
    OUTPUT_DIR=$(cd -- "$RENDER_DIR" && pwd)
else
    OUTPUT_DIR="$TEMP_DIR/rendered"
    mkdir -p "$OUTPUT_DIR"
fi

write_proxy_locations() {
    cat <<EOF
    location = /healthz { return 404; }
    location ^~ /v1/webhook/ {
        if (\$http_authorization = "") { return 403; }
        limit_req zone=${SERVICE_NAME//-/_}_webhook burst=$WEBHOOK_BURST nodelay;
        limit_req_status 429;
        client_max_body_size $MAX_CONTENT_LENGTH;
        proxy_pass http://unix:$API_SOCKET_PATH;
        include proxy_params;
        proxy_read_timeout 15s;
        proxy_connect_timeout 5s;
    }
    location ^~ /v1/network {
        limit_req zone=${SERVICE_NAME//-/_}_network burst=$NETWORK_BURST nodelay;
        limit_req_status 429;
        proxy_pass http://unix:$API_SOCKET_PATH;
        include proxy_params;
        proxy_read_timeout 15s;
        proxy_connect_timeout 5s;
    }
    location / {
        limit_req zone=${SERVICE_NAME//-/_}_network burst=$NETWORK_BURST nodelay;
        limit_req_status 429;
        proxy_pass http://unix:$API_SOCKET_PATH;
        include proxy_params;
        proxy_read_timeout 15s;
        proxy_connect_timeout 5s;
    }
    error_page 429 = @rate_limited;
    location @rate_limited {
        default_type application/json;
        add_header Retry-After 1 always;
        add_header Access-Control-Allow-Origin "*" always;
        return 429 '{"error":"Too many requests"}';
    }
EOF
}
write_nginx() {
    local tls=$1
    cat <<EOF
# Included from nginx.conf's http context; do not install inside a server block.
limit_req_zone \$binary_remote_addr zone=${SERVICE_NAME//-/_}_network:1m rate=${NETWORK_RATE}r/s;
limit_req_zone \$binary_remote_addr zone=${SERVICE_NAME//-/_}_webhook:1m rate=${WEBHOOK_RATE}r/s;
server {
    listen 80;
    listen [::]:80;
    server_name $DOMAIN;
$PROXY_CONFIG
    location ^~ /.well-known/acme-challenge/ { root /var/lib/letsencrypt; }
EOF
    if [[ $tls == true ]]; then
        cat <<EOF
    location / { return 301 https://$DOMAIN\$request_uri; }
}
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name $DOMAIN;
    ssl_certificate /etc/letsencrypt/live/$DOMAIN/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/$DOMAIN/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_session_cache shared:${SERVICE_NAME//-/_}_ssl:1m;
    ssl_session_timeout 1d;
$PROXY_CONFIG
EOF
    fi
    printf '%s\n' '    client_header_timeout 10s;' '    client_body_timeout 10s;' '    send_timeout 15s;' '    keepalive_timeout 15s;' '    server_tokens off;'
    write_proxy_locations
    printf '}\n'
}

cat > "$OUTPUT_DIR/jcb-api.env" <<EOF
# Generated deployment settings. Production credentials live in TOKENS_FILE_PATH.
CACHE_FILE_PATH=$STATE_DIR/cache.json
TOKENS_FILE_PATH=$STATE_DIR/tokens.json
TEMP_CACHE_FILE_PATH=$STATE_DIR/cache_temp.json
API_SOCKET_PATH=$API_SOCKET_PATH
ADMIN_SOCKET_PATH=$ADMIN_SOCKET_PATH
SERVER_MEMORY_MB=$SERVER_MEMORY_MB
SERVER_CPU_COUNT=$SERVER_CPU_COUNT
MAX_STATE_BYTES=$MAX_STATE_BYTES
MAX_CONTENT_LENGTH=$MAX_CONTENT_LENGTH
LOG_NETWORK_REQUESTS=$LOG_NETWORK_REQUESTS
LOG_QUEUE_CAPACITY=$LOG_QUEUE_CAPACITY
POLL_INTERVAL_SECONDS=$POLL_INTERVAL_SECONDS
PUBLIC_BASE_URL=https://$DOMAIN
SERVICE_NAME=$SERVICE_NAME.service
EOF
if ! $ENABLE_HTTPS; then sed -i "s|PUBLIC_BASE_URL=https://|PUBLIC_BASE_URL=http://|" "$OUTPUT_DIR/jcb-api.env"; fi

cat > "$OUTPUT_DIR/$SERVICE_NAME.service" <<EOF
[Unit]
Description=JCB network availability API
After=network.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
User=$SERVICE_USER
Group=$NGINX_GROUP
WorkingDirectory=$STATE_DIR
ExecStart=$INSTALL_DIR/current/jcb-api --env-file $CONFIG_DIR/.env serve
Restart=on-failure
RestartSec=3
TimeoutStopSec=30
RuntimeDirectory=$SERVICE_NAME
RuntimeDirectoryMode=0750
UMask=0077
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
ProtectKernelLogs=true
RestrictSUIDSGID=true
RestrictRealtime=true
LockPersonality=true
RestrictAddressFamilies=AF_UNIX
CapabilityBoundingSet=
AmbientCapabilities=
ReadWritePaths=$STATE_DIR $RUNTIME_DIR
MemoryHigh=${MEMORY_HIGH_MB}M
MemoryMax=${SERVICE_MEMORY_MB}M
CPUQuota=$((SERVER_CPU_COUNT * 100))%
TasksMax=$TASKS_MAX
LimitNOFILE=65536
StandardOutput=journal
StandardError=journal
SyslogIdentifier=$SERVICE_NAME
LogRateLimitIntervalSec=0

[Install]
WantedBy=multi-user.target
EOF

cat > "$OUTPUT_DIR/$SERVICE_NAME-update.service" <<EOF
[Unit]
Description=Install verified stable JCB API releases
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
User=root
UMask=0077
ExecStart=$INSTALL_DIR/current/jcb-api update --repository $REPOSITORY --install-dir $INSTALL_DIR --public-key $CONFIG_DIR/release-public.pem --socket $API_SOCKET_PATH --service $SERVICE_NAME.service
TimeoutStartSec=15min
Nice=10
IOSchedulingClass=idle
PrivateTmp=true
NoNewPrivileges=true
ProtectHome=read-only
EOF

cat > "$OUTPUT_DIR/$SERVICE_NAME-update.timer" <<EOF
[Unit]
Description=Check periodically for stable JCB API releases

[Timer]
OnBootSec=5min
OnUnitActiveSec=$UPDATE_INTERVAL
RandomizedDelaySec=60
Persistent=true
Unit=$SERVICE_NAME-update.service

[Install]
WantedBy=timers.target
EOF

write_nginx false > "$OUTPUT_DIR/nginx-http.conf"
write_nginx "$ENABLE_HTTPS" > "$OUTPUT_DIR/nginx.conf"
cat > "$OUTPUT_DIR/nginx-acme.conf" <<EOF
# Temporary certificate-enrollment vhost: application routes remain closed.
server {
    listen 80;
    listen [::]:80;
    server_name $DOMAIN;
    location ^~ /.well-known/acme-challenge/ { root /var/lib/letsencrypt; }
    location / { return 503; }
}
EOF
for key in REPOSITORY DOMAIN INSTALL_DIR STATE_DIR CONFIG_DIR SERVICE_NAME SERVICE_USER NGINX_GROUP SERVER_MEMORY_MB SERVER_CPU_COUNT NETWORK_RATE NETWORK_BURST WEBHOOK_RATE WEBHOOK_BURST AUTO_UPDATE UPDATE_INTERVAL ENABLE_HTTPS ACME_EMAIL LEGACY_ENV_FILE TRUSTED_PROXY_CIDRS LOG_NETWORK_REQUESTS LOG_QUEUE_CAPACITY MAX_CONTENT_LENGTH MAX_STATE_BYTES POLL_INTERVAL_SECONDS; do
    printf '%s=%s\n' "$key" "${!key}"
done > "$OUTPUT_DIR/install.conf"
printf 'PUBLIC_KEY_FILE=%s/release-public.pem\nRELEASE_VERSION=latest\n' "$CONFIG_DIR" >> "$OUTPUT_DIR/install.conf"

if [[ -n $RENDER_DIR ]]; then
    note "Validated configuration and rendered deployment files in $OUTPUT_DIR"
    exit 0
fi

((EUID == 0)) || die 'Installation requires root; use sudo bash install.sh'
[[ -f /etc/debian_version ]] || die 'This installer supports Debian and Ubuntu with systemd'
need systemctl
need apt-get
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl jq openssl nginx
need curl; need jq; need openssl; need sha256sum

# Release/config parents must not allow the runtime account (or another ordinary
# user) to replace a path later traversed by the root-owned updater.
for secure_path in "$INSTALL_DIR" "$CONFIG_DIR"; do
    [[ $(realpath -m -- "$secure_path") == "$secure_path" ]] || die "Symlink components are not allowed in $secure_path"
    ancestor=$secure_path
    while [[ ! -e $ancestor ]]; do ancestor=$(dirname -- "$ancestor"); done
    while [[ $ancestor != / ]]; do
        [[ $(stat -c '%u' -- "$ancestor") == 0 ]] || die "Root-managed installation/configuration parents must be root-owned: $ancestor"
        mode=$(stat -c '%a' -- "$ancestor")
        (( (8#$mode & 0022) == 0 )) || die "Root-managed installation/configuration parents must not be group/world writable: $ancestor"
        ancestor=$(dirname -- "$ancestor")
    done
done
[[ $(realpath -m -- "$STATE_DIR") == "$STATE_DIR" ]] || die 'Symlink components are not allowed in the state directory'

# Do not silently leave a legacy vhost serving the same name ahead of this one.
# The operator may disable that symlink while retaining its original file.
for site in /etc/nginx/sites-enabled/*; do
    [[ -f $site && $site != "/etc/nginx/sites-enabled/$SERVICE_NAME" ]] || continue
    if awk -v domain="$DOMAIN" '$1 == "server_name" { for (i=2; i<=NF; i++) { sub(/;$/, "", $i); if ($i == domain) found=1 } } END { exit !found }' "$site"; then
        die "Domain $DOMAIN is already configured in $site. Disable that legacy site symlink before installing; keep its source file for rollback."
    fi
done

case $(uname -m) in
    x86_64) TARGET=x86_64-unknown-linux-musl ;;
    aarch64|arm64) TARGET=aarch64-unknown-linux-musl ;;
    *) die 'No prebuilt release is supported for this CPU architecture' ;;
esac
API_URL="https://api.github.com/repos/$REPOSITORY/releases"
if [[ $RELEASE_VERSION == latest ]]; then API_URL+=/latest; else API_URL+="/tags/$RELEASE_VERSION"; fi
curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --connect-timeout 10 --max-time 60 "$API_URL" -o "$TEMP_DIR/release.json"
TAG=$(jq -er '.tag_name | select(test("^v[0-9]+\\.[0-9]+\\.[0-9]+$"))' "$TEMP_DIR/release.json") || die 'GitHub returned an invalid stable release'
jq -e '.draft == false and .prerelease == false' "$TEMP_DIR/release.json" >/dev/null || die 'Refusing a draft or prerelease'
ASSET="jcb-api-$TARGET"
for asset in SHA256SUMS SHA256SUMS.sig "$ASSET"; do
    # Construct URLs from the validated repository, tag and fixed asset names.
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --connect-timeout 10 --max-time 180 --max-filesize 67108864 \
        "https://github.com/$REPOSITORY/releases/download/$TAG/$asset" -o "$TEMP_DIR/$asset"
done
[[ $(wc -c < "$TEMP_DIR/SHA256SUMS.sig") -eq 64 ]] || die 'Release signature must be 64 Ed25519 bytes'
PUBLIC_KEY_INFO=$(openssl pkey -pubin -in "$PUBLIC_KEY_FILE" -text -noout) || die 'Invalid release public key'
[[ $PUBLIC_KEY_INFO == *ED25519* ]] || die 'Release key must be Ed25519'
openssl pkeyutl -verify -pubin -inkey "$PUBLIC_KEY_FILE" -rawin -in "$TEMP_DIR/SHA256SUMS" -sigfile "$TEMP_DIR/SHA256SUMS.sig" >/dev/null || die 'Release signature verification failed'
DIGEST=$(awk -v name="$ASSET" '$2 == name && length($1) == 64 && $1 ~ /^[0-9a-f]+$/ { print $1; count++ } END { if (count != 1) exit 1 }' "$TEMP_DIR/SHA256SUMS") || die 'Signed manifest must contain exactly one matching asset'
ACTUAL_DIGEST=$(sha256sum "$TEMP_DIR/$ASSET"); ACTUAL_DIGEST=${ACTUAL_DIGEST%% *}
[[ $DIGEST == "$ACTUAL_DIGEST" ]] || die 'Release binary checksum verification failed'
chmod 0755 "$TEMP_DIR/$ASSET"
[[ $("$TEMP_DIR/$ASSET" --version) == "jcb-api ${TAG#v}" ]] || die 'Signed binary version does not match the release tag'

getent group "$NGINX_GROUP" >/dev/null || die "Nginx group does not exist: $NGINX_GROUP"
if ! id "$SERVICE_USER" >/dev/null 2>&1; then
    useradd --system --user-group --home-dir "$STATE_DIR" --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
else
    [[ $(id -u "$SERVICE_USER") != 0 ]] || die 'The application cannot run as root'
    ACCOUNT_SHELL=$(getent passwd "$SERVICE_USER" | cut -d: -f7)
    [[ $ACCOUNT_SHELL == */nologin || $ACCOUNT_SHELL == */false ]] || die 'SERVICE_USER must be a dedicated account with login disabled'
fi
install -d -m 0755 -o root -g root "$INSTALL_DIR" "$INSTALL_DIR/releases" "$INSTALL_DIR/releases/$TAG"
install -d -m 0750 -o root -g "$NGINX_GROUP" "$CONFIG_DIR"
install -d -m 0700 -o "$SERVICE_USER" -g "$NGINX_GROUP" "$STATE_DIR"

# Never overwrite runtime data or mutate the original installation.
LEGACY_CACHE=''
LEGACY_TOKENS=''
if [[ -n $LEGACY_ENV_FILE ]]; then
    [[ -f $LEGACY_ENV_FILE && ! -L $LEGACY_ENV_FILE ]] || die 'Legacy .env must be a regular file'
    while IFS= read -r line || [[ -n $line ]]; do
        trim "$line"; line=$REPLY
        [[ $line =~ ^(CACHE_FILE_PATH|TOKENS_FILE_PATH)[[:space:]]*=(.*)$ ]] || continue
        key=${BASH_REMATCH[1]}; value=${BASH_REMATCH[2]}
        parse_value "$value"; valid_path "$REPLY"
        if [[ $key == CACHE_FILE_PATH ]]; then LEGACY_CACHE=$REPLY; else LEGACY_TOKENS=$REPLY; fi
    done < "$LEGACY_ENV_FILE"
    [[ -n $LEGACY_CACHE && -n $LEGACY_TOKENS ]] || die 'Legacy .env must specify CACHE_FILE_PATH and TOKENS_FILE_PATH'
fi
for kind in cache tokens; do
    target="$STATE_DIR/$kind.json"
    if [[ -e $target ]]; then
        [[ -f $target && ! -L $target ]] || die "Refusing non-regular state file: $target"
        note "Preserving existing $kind data"
    else
        if [[ $kind == cache ]]; then source=$LEGACY_CACHE; else source=$LEGACY_TOKENS; fi
        if [[ -n $source ]]; then
            [[ -f $source && ! -L $source ]] || die "Migration source must be a regular file: $source"
            jq -e 'type == "object"' "$source" >/dev/null || die "Invalid JSON object in $source"
            install -m 0600 -o "$SERVICE_USER" -g "$NGINX_GROUP" "$source" "$target"
        else
            if [[ $kind == cache ]]; then printf '{"systems":[]}\n' > "$target"; else printf '{}\n' > "$target"; fi
            chown "$SERVICE_USER:$NGINX_GROUP" "$target"
            chmod 0600 "$target"
        fi
    fi
done

if [[ -e $CONFIG_DIR/.env ]]; then
    [[ -f $CONFIG_DIR/.env && ! -L $CONFIG_DIR/.env ]] || die 'Existing environment file is not a regular file'
    install -m 0600 "$CONFIG_DIR/.env" "$CONFIG_DIR/.env.before-install"
fi
install -m 0640 -o root -g "$NGINX_GROUP" "$OUTPUT_DIR/jcb-api.env" "$CONFIG_DIR/.env"
if [[ $(readlink -f "$PUBLIC_KEY_FILE") != "$CONFIG_DIR/release-public.pem" ]]; then
    install -m 0644 -o root -g root "$PUBLIC_KEY_FILE" "$CONFIG_DIR/release-public.pem"
fi
install -m 0600 -o root -g root "$OUTPUT_DIR/install.conf" "$CONFIG_DIR/install.conf"
if [[ -e $INSTALL_DIR/releases/$TAG/jcb-api ]]; then
    [[ ! -L $INSTALL_DIR/releases/$TAG/jcb-api ]] || die 'Release binaries must not be symlinks'
    EXISTING_DIGEST=$(sha256sum "$INSTALL_DIR/releases/$TAG/jcb-api"); EXISTING_DIGEST=${EXISTING_DIGEST%% *}
    [[ $EXISTING_DIGEST == "$DIGEST" ]] || die 'Refusing to replace an existing release with different bytes'
else
    install -m 0755 -o root -g root "$TEMP_DIR/$ASSET" "$INSTALL_DIR/releases/$TAG/jcb-api"
fi
runuser -u "$SERVICE_USER" -g "$NGINX_GROUP" -- test -r "$CONFIG_DIR/.env" || die 'The service account cannot traverse the configuration path'
runuser -u "$SERVICE_USER" -g "$NGINX_GROUP" -- test -x "$INSTALL_DIR/releases/$TAG/jcb-api" || die 'The service account cannot traverse the installation path'

PREVIOUS_RELEASE=''
if [[ -L $INSTALL_DIR/current ]]; then PREVIOUS_RELEASE=$(readlink "$INSTALL_DIR/current"); elif [[ -e $INSTALL_DIR/current ]]; then die 'current must be a symlink'; fi
[[ ! -e $INSTALL_DIR/.current-new && ! -L $INSTALL_DIR/.current-new ]] || die 'Stale .current-new entry exists; inspect and remove it before retrying'
ln -s "releases/$TAG" "$INSTALL_DIR/.current-new"
mv -Tf "$INSTALL_DIR/.current-new" "$INSTALL_DIR/current"
for unit in "$SERVICE_NAME.service" "$SERVICE_NAME-update.service" "$SERVICE_NAME-update.timer"; do
    if [[ -f /etc/systemd/system/$unit ]]; then cp -p -- "/etc/systemd/system/$unit" "$TEMP_DIR/$unit.previous"; fi
    install -m 0644 "$OUTPUT_DIR/$unit" "/etc/systemd/system/$unit"
done
systemctl daemon-reload
systemctl enable "$SERVICE_NAME.service"
if ! systemctl restart "$SERVICE_NAME.service"; then note 'Service restart failed; checking health before rollback.'; fi
healthy=false
for ((attempt=0; attempt<30; attempt++)); do
    if curl --fail --silent --max-time 1 --unix-socket "$API_SOCKET_PATH" http://localhost/healthz >/dev/null; then healthy=true; break; fi
    sleep 1
done
if ! $healthy; then
    if [[ -n $PREVIOUS_RELEASE ]]; then
        ln -s "$PREVIOUS_RELEASE" "$INSTALL_DIR/.current-rollback"
        mv -Tf "$INSTALL_DIR/.current-rollback" "$INSTALL_DIR/current"
        if [[ -f $CONFIG_DIR/.env.before-install ]]; then install -m 0640 -o root -g "$NGINX_GROUP" "$CONFIG_DIR/.env.before-install" "$CONFIG_DIR/.env"; fi
        if [[ -f $TEMP_DIR/$SERVICE_NAME.service.previous ]]; then install -m 0644 "$TEMP_DIR/$SERVICE_NAME.service.previous" "/etc/systemd/system/$SERVICE_NAME.service"; fi
        systemctl daemon-reload
        systemctl restart "$SERVICE_NAME.service"
    fi
    die 'New service did not become healthy; previous binary restored when available. Inspect journalctl.'
fi

install -d -m 0755 /var/lib/letsencrypt/.well-known/acme-challenge
NGINX_TARGET="/etc/nginx/sites-available/$SERVICE_NAME"
if [[ -e $NGINX_TARGET ]]; then cp -p -- "$NGINX_TARGET" "$NGINX_TARGET.before-install"; fi
if $ENABLE_HTTPS; then INITIAL_NGINX="$OUTPUT_DIR/nginx-acme.conf"; else INITIAL_NGINX="$OUTPUT_DIR/nginx-http.conf"; fi
install -m 0644 "$INITIAL_NGINX" "$NGINX_TARGET"
ln -sfn "$NGINX_TARGET" "/etc/nginx/sites-enabled/$SERVICE_NAME"
nginx -t
systemctl enable --now nginx
systemctl reload nginx
if $ENABLE_HTTPS; then
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends certbot
    certbot certonly --webroot --webroot-path /var/lib/letsencrypt --domain "$DOMAIN" --email "$ACME_EMAIL" --agree-tos --non-interactive --keep-until-expiring
    install -m 0644 "$OUTPUT_DIR/nginx.conf" "$NGINX_TARGET"
    nginx -t
    systemctl reload nginx
    install -d -m 0755 /etc/letsencrypt/renewal-hooks/deploy
    printf '#!/bin/sh\n/usr/bin/systemctl reload nginx\n' > /etc/letsencrypt/renewal-hooks/deploy/jcb-api-nginx
    chmod 0755 /etc/letsencrypt/renewal-hooks/deploy/jcb-api-nginx
fi
if $AUTO_UPDATE; then systemctl enable --now "$SERVICE_NAME-update.timer"; else systemctl disable --now "$SERVICE_NAME-update.timer" 2>/dev/null || true; fi
note "Installed $TAG for $DOMAIN. Existing source cache, tokens and logs were not changed."
note "Follow activity: sudo journalctl -u $SERVICE_NAME.service -f"
note "Saved deployment settings: $CONFIG_DIR/install.conf"
note "Administrative CLI: sudo $INSTALL_DIR/current/jcb-api --env-file $CONFIG_DIR/.env --help"
