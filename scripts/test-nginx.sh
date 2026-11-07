#!/usr/bin/env bash
# Real Nginx + Rust integration on loopback, using generated deployment rules.
# This script never changes system configuration or contacts production.
set -Eeuo pipefail
binary=$(realpath -- "${1:-target/debug/jcb-api}")
[[ -x "$binary" ]] || { printf 'Executable not found: %s\n' "$binary" >&2; exit 1; }
command -v nginx > /dev/null
workspace=$(mktemp -d)
server_pid=
nginx_pid=
port=${JCB_NGINX_TEST_PORT:-18081}
[[ "$port" =~ ^[0-9]+$ ]] && ((port >= 1024 && port <= 65535)) || {
    printf 'JCB_NGINX_TEST_PORT must be an unprivileged TCP port.\n' >&2
    exit 1
}
mkdir -p target
diagnostics=$(realpath target)/ci-nginx.log
cleanup() {
    trap - ERR
    if [[ -n "$nginx_pid" ]] && kill -0 "$nginx_pid" 2>/dev/null; then
        kill -QUIT "$nginx_pid"
        wait "$nginx_pid" || true
    fi
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill -TERM "$server_pid"
        wait "$server_pid" || true
    fi
    {
        for log in service.log nginx.log nginx-error.log access.log; do
            printf '\n%s\n' "$log"
            [[ ! -f "$workspace/$log" ]] || cat "$workspace/$log"
        done
    } > "$diagnostics"
    rm -rf -- "$workspace"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
on_error() {
    local status=$1 line=$2
    trap - ERR
    printf 'Nginx integration failed at line %s (exit %s).\n' "$line" "$status" >&2
    for log in service.log nginx.log nginx-error.log access.log; do
        [[ ! -f "$workspace/$log" ]] || tail -n 40 "$workspace/$log" >&2
    done
    exit "$status"
}
trap 'on_error "$?" "$LINENO"' ERR
fail() { printf '%s\n' "$*" >&2; on_error 1 "$LINENO"; }

printf '%s\n' '{"systems":[]}' > "$workspace/cache.json"
printf '%s\n' '{"community":["nginx-integration-token"]}' > "$workspace/tokens.json"
cat > "$workspace/.env" <<EOF
CACHE_FILE_PATH=$workspace/cache.json
TOKENS_FILE_PATH=$workspace/tokens.json
API_SOCKET_PATH=$workspace/api.sock
ADMIN_SOCKET_PATH=$workspace/admin.sock
SERVER_MEMORY_MB=512
SERVER_CPU_COUNT=2
LOG_NETWORK_REQUESTS=true
EOF
"$binary" --env-file "$workspace/.env" serve > "$workspace/service.log" 2>&1 &
server_pid=$!
ready=false
for ((attempt=0; attempt<100; attempt++)); do
    kill -0 "$server_pid" 2>/dev/null || fail 'Rust service exited during startup'
    if [[ -S "$workspace/api.sock" ]] && curl --silent --max-time 1 --unix-socket "$workspace/api.sock" \
        http://localhost/healthz > "$workspace/health.json"; then
        ready=true
        break
    fi
    sleep 0.1
done
[[ "$ready" == true ]] || fail 'Rust service did not become healthy'
jq -e '.status == "ok"' "$workspace/health.json" > /dev/null

printf 'Render-only public key fixture\n' > "$workspace/public.pem"
cat > "$workspace/deploy.conf" <<EOF
DOMAIN=api.example.org
PUBLIC_KEY_FILE=$workspace/public.pem
ENABLE_HTTPS=false
SERVER_MEMORY_MB=512
SERVER_CPU_COUNT=2
EOF
bash scripts/install.sh --config "$workspace/deploy.conf" --non-interactive --render-only "$workspace/render"
# Retain all production locations/rate rules. Only transport addresses are moved
# into the isolated test instance, and IPv6's second production listener omitted.
sed -e "s/listen 80;/listen 127.0.0.1:$port;/" \
    -e '/listen \[::\]:80;/d' \
    -e "s|http://unix:/run/jcb-api/api.sock|http://unix:$workspace/api.sock|g" \
    "$workspace/render/nginx.conf" > "$workspace/site.conf"
cp /etc/nginx/proxy_params "$workspace/proxy_params"
cat > "$workspace/nginx.conf" <<EOF
worker_processes 1;
error_log $workspace/nginx-error.log;
pid $workspace/nginx.pid;
events { worker_connections 512; }
http {
    log_format upstream_trace '\$request_uri \$status \$upstream_status';
    access_log $workspace/access.log upstream_trace;
    client_body_temp_path $workspace/client-body;
    proxy_temp_path $workspace/proxy-temp;
    fastcgi_temp_path $workspace/fastcgi-temp;
    uwsgi_temp_path $workspace/uwsgi-temp;
    scgi_temp_path $workspace/scgi-temp;
    include $workspace/site.conf;
}
EOF
if ((EUID == 0)); then
    # Local root-driven tests retain access to their private temporary directory.
    printf 'user %s %s;\n' "$(id -un)" "$(id -gn)" >> "$workspace/nginx.conf"
fi
nginx -t -p "$workspace/" -c "$workspace/nginx.conf" -e "$workspace/nginx-error.log"
nginx -p "$workspace/" -c "$workspace/nginx.conf" -e "$workspace/nginx-error.log" -g 'daemon off;' \
    > "$workspace/nginx.log" 2>&1 &
nginx_pid=$!
edge() { curl --silent --show-error --max-time 5 --header 'Host: api.example.org' "$@"; }
base_url="http://127.0.0.1:$port"
ready=false
for ((attempt=0; attempt<100; attempt++)); do
    kill -0 "$nginx_pid" 2>/dev/null || fail 'Nginx exited during startup'
    if edge "$base_url/openapi.json" > "$workspace/openapi.json" 2>/dev/null; then
        ready=true
        break
    fi
    sleep 0.1
done
[[ "$ready" == true ]] || fail 'Nginx did not become ready'
jq -e '.openapi | startswith("3.")' "$workspace/openapi.json" > /dev/null
expect_code() {
    local expected=$1
    shift
    local actual
    actual=$(edge --output "$workspace/response.json" --write-out '%{http_code}' "$@")
    [[ "$actual" == "$expected" ]] || fail "Expected $expected, received $actual"
}
assert_access() {
    for ((attempt=0; attempt<20; attempt++)); do
        if grep -Fq -- "$1" "$workspace/access.log"; then return; fi
        sleep 0.05
    done
    fail "Expected Nginx upstream evidence: $1"
}

payload='{"heartbeat":{"monitorID":7,"status":1},"monitor":{"name":"Edge test","url":"https://example.invalid/mirror"}}'
expect_code 403 -X POST -H 'Content-Type: application/json' --data "$payload" \
    "$base_url/v1/webhook/community/jcb/edge?nginx-test=missing"
# A dash in upstream_status proves the rejection happened in Nginx, not Rust.
assert_access '/v1/webhook/community/jcb/edge?nginx-test=missing 403 -'
expect_code 200 -X POST -H 'Content-Type: application/json' -H 'Authorization: nginx-integration-token' \
    --data "$payload" "$base_url/v1/webhook/community/jcb/edge?nginx-test=valid"
assert_access '/v1/webhook/community/jcb/edge?nginx-test=valid 200 200'
expect_code 200 "$base_url/v1/network/community/jcb/edge"
jq -e '.network[0].id == 7 and .network[0].status == 1' "$workspace/response.json" > /dev/null
expect_code 404 "$base_url/healthz?nginx-test=private"
assert_access '/healthz?nginx-test=private 404 -'

# Generated default 100r/s plus burst500 must admit this isolated compilation.
export JCB_NGINX_SMOKE_URL="$base_url/v1/network/community/jcb/edge"
seq 1 100 | xargs -P 32 -I '{}' sh -c \
    'test "$(curl --silent --show-error --max-time 5 --header "Host: api.example.org" -o /dev/null -w "%{http_code}" "$JCB_NGINX_SMOKE_URL")" = 200'
printf 'Generated Nginx rules reject tokenless webhooks at the edge, proxy valid requests, hide health, and permit a 100-call burst.\n'
