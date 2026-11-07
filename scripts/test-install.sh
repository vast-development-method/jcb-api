#!/usr/bin/env bash
# Offline installer validation. No system changes, credentials, or downloads.
set -Eeuo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
TEST_DIR=$(mktemp -d)
trap 'rm -rf -- "$TEST_DIR"' EXIT
mkdir -p "$TEST_DIR/source"
printf 'public key data is not executed in render mode\n' > "$TEST_DIR/source/release-public.pem"
cat > "$TEST_DIR/base.conf" <<EOF
DOMAIN=api.example.org
PUBLIC_KEY_FILE=$TEST_DIR/source/release-public.pem
ENABLE_HTTPS=false
SERVER_MEMORY_MB=512 # supported inline comment
SERVER_CPU_COUNT=2
EOF

require_text() { [[ $(<"$1") == *"$2"* ]] || { printf 'Missing expected text in %s: %s\n' "$1" "$2" >&2; exit 1; }; }
reject() {
    local key=$1 value=$2
    cp "$TEST_DIR/base.conf" "$TEST_DIR/bad.conf"
    printf '%s=%s\n' "$key" "$value" >> "$TEST_DIR/bad.conf"
    if bash scripts/install.sh --config "$TEST_DIR/bad.conf" --non-interactive --render-only "$TEST_DIR/bad" >"$TEST_DIR/stdout" 2>"$TEST_DIR/stderr"; then
        printf 'Unsafe configuration unexpectedly accepted: %s\n' "$key" >&2
        exit 1
    fi
}
bash -n scripts/install.sh
bash scripts/install.sh --config "$TEST_DIR/base.conf" --non-interactive --render-only "$TEST_DIR/http"
require_text "$TEST_DIR/http/jcb-api.service" 'MemoryMax=256M'
require_text "$TEST_DIR/http/jcb-api.service" 'CPUQuota=200%'
require_text "$TEST_DIR/http/jcb-api.service" 'RestrictAddressFamilies=AF_UNIX'
require_text "$TEST_DIR/http/jcb-api.service" 'User=jcb-api'
require_text "$TEST_DIR/http/jcb-api.service" 'Group=www-data'
require_text "$TEST_DIR/http/jcb-api.env" 'MAX_STATE_BYTES=8388608'
require_text "$TEST_DIR/http/jcb-api.env" 'PUBLIC_BASE_URL=http://api.example.org'
require_text "$TEST_DIR/http/jcb-api.env" 'API_SOCKET_PATH=/run/jcb-api/api.sock'
require_text "$TEST_DIR/http/jcb-api-update.service" '--public-key /etc/jcb-api/release-public.pem'
require_text "$TEST_DIR/http/nginx.conf" 'if ($http_authorization = "") { return 403; }'
require_text "$TEST_DIR/http/nginx.conf" 'burst=500 nodelay;'
require_text "$TEST_DIR/http/nginx.conf" 'limit_req_status 429;'
require_text "$TEST_DIR/http/nginx.conf" 'add_header Retry-After 1 always;'
require_text "$TEST_DIR/http/nginx.conf" 'location = /healthz { return 404; }'
[[ $(<"$TEST_DIR/http/nginx.conf") != *set_real_ip_from* ]] || { printf 'Default trusts forwarded headers\n' >&2; exit 1; }

cat "$TEST_DIR/base.conf" > "$TEST_DIR/https.conf"
cat >> "$TEST_DIR/https.conf" <<'EOF'
ENABLE_HTTPS=true
ACME_EMAIL='maintainer@example.org'
TRUSTED_PROXY_CIDRS=192.0.2.0/24,2001:db8::/32
INSTALL_DIR=/opt/custom-api
CONFIG_DIR=/etc/custom-api
STATE_DIR=/var/lib/custom-api
SERVICE_NAME=custom-api
SERVICE_USER=custom-api
SERVER_MEMORY_MB=1024
SERVER_CPU_COUNT=4
EOF
bash scripts/install.sh --config "$TEST_DIR/https.conf" --non-interactive --render-only "$TEST_DIR/https"
require_text "$TEST_DIR/https/custom-api.service" 'MemoryMax=512M'
require_text "$TEST_DIR/https/custom-api.service" 'CPUQuota=400%'
require_text "$TEST_DIR/https/nginx.conf" 'listen 443 ssl;'
require_text "$TEST_DIR/https/nginx.conf" 'set_real_ip_from 192.0.2.0/24;'
require_text "$TEST_DIR/https/nginx.conf" 'set_real_ip_from 2001:db8::/32;'
require_text "$TEST_DIR/https/nginx.conf" 'proxy_pass http://unix:/run/custom-api/api.sock;'

reject DOMAIN 'example.org; touch /tmp/should-not-exist'
reject DOMAIN 'example..org'
reject INSTALL_DIR '/tmp/$(touch /tmp/should-not-exist)'
reject INSTALL_DIR '/opt/../etc'
reject INSTALL_DIR '/etc'
reject STATE_DIR '/opt/jcb-api/state'
reject SERVICE_USER root
reject SERVER_MEMORY_MB 128
reject SERVER_MEMORY_MB 000512
reject SERVER_CPU_COUNT 0
reject NETWORK_BURST 99
reject AUTO_UPDATE yes
reject UPDATE_INTERVAL '15min; touch /tmp/should-not-exist'
reject TRUSTED_PROXY_CIDRS '192.0.2.1;return 200'
reject RELEASE_VERSION '../main'
reject UNSUPPORTED_KEY value
reject ENABLE_HTTPS true
reject MAX_STATE_BYTES 999999999

# Optional native parser validation in CI environments that install nginx.
if command -v nginx >/dev/null 2>&1; then
    cat > "$TEST_DIR/nginx-test.conf" <<EOF
error_log $TEST_DIR/nginx-error.log;
pid $TEST_DIR/nginx.pid;
events { worker_connections 64; }
http {
    access_log off;
    client_body_temp_path $TEST_DIR/client-body;
    proxy_temp_path $TEST_DIR/proxy-temp;
    fastcgi_temp_path $TEST_DIR/fastcgi-temp;
    uwsgi_temp_path $TEST_DIR/uwsgi-temp;
    scgi_temp_path $TEST_DIR/scgi-temp;
    include $TEST_DIR/http/nginx.conf;
}
EOF
    nginx -t -p /etc/nginx/ -c "$TEST_DIR/nginx-test.conf"
fi
printf 'Installer render, resource, routing and injection checks passed.\n'
