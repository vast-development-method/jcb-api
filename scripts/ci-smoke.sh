#!/usr/bin/env bash
# End-to-end checks against a temporary local instance. Never contacts production.
set -Eeuo pipefail

binary=$(realpath -- "${1:-target/debug/jcb-api}")
[[ -x "$binary" ]] || { printf 'Executable not found: %s\n' "$binary" >&2; exit 1; }
workspace=$(mktemp -d)
server_pid=
mkdir -p target
diagnostics=$(realpath target)/ci-smoke.log
cleanup() {
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill -TERM "$server_pid"
        wait "$server_pid" || true
    fi
    if [[ -f "$workspace/service.log" ]]; then
        cp "$workspace/service.log" "$diagnostics"
    fi
    rm -rf -- "$workspace"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
on_error() {
    local status=$1 line=$2
    trap - ERR
    printf 'Unexpected smoke-test failure at line %s (exit %s).\n' "$line" "$status" >&2
    if [[ -f "$workspace/service.log" ]]; then
        tail -n 100 "$workspace/service.log" >&2
    fi
    exit "$status"
}
trap 'on_error "$?" "$LINENO"' ERR

fail() { printf 'Smoke test failed: %s\n' "$*" >&2; cat "$workspace/service.log" >&2 2>/dev/null || true; exit 1; }
api() { curl --silent --show-error --max-time 5 --unix-socket "$workspace/api.sock" "$@"; }
cli() { "$binary" --env-file "$workspace/.env" "$@"; }
expect_code() {
    local expected=$1
    shift
    local actual
    actual=$(api -o "$workspace/response.json" -w '%{http_code}' "$@")
    [[ "$actual" == "$expected" ]] || fail "expected HTTP $expected, received $actual for $*"
}
start_service() {
    # Background the executable itself: a background shell function creates a
    # wrapper PID, and signalling that wrapper would leave the service running.
    "$binary" --env-file "$workspace/.env" serve >> "$workspace/service.log" 2>&1 &
    server_pid=$!
    local ready=false
    for ((attempt = 0; attempt < 100; attempt++)); do
        kill -0 "$server_pid" 2>/dev/null || fail 'service exited during startup'
        if [[ -S "$workspace/api.sock" ]] && api http://localhost/healthz > "$workspace/health.json" 2>/dev/null; then
            ready=true
            break
        fi
        sleep 0.1
    done
    [[ "$ready" == true ]] || fail 'service did not become healthy'
    jq -e '.status == "ok" and (.version | type == "string")' "$workspace/health.json" > /dev/null
}
stop_service() {
    kill -TERM "$server_pid"
    for ((attempt = 0; attempt < 100; attempt++)); do
        if ! kill -0 "$server_pid" 2>/dev/null; then
            wait "$server_pid"
            server_pid=
            return
        fi
        sleep 0.1
    done
    fail 'service did not shut down gracefully'
}

cat > "$workspace/cache.json" <<'JSON'
{"systems":[{"system":"community","projects":[{"project":"jcb","targets":[{"target":"legacy","network":[{"id":1,"name":"Legacy mirror","url":"https://example.invalid/legacy","status":1}]}]}]}]}
JSON
printf '%s\n' '{"community":["legacy-smoke-token"]}' > "$workspace/tokens.json"
cat > "$workspace/.env" <<EOF
CACHE_FILE_PATH=$workspace/cache.json
TOKENS_FILE_PATH=$workspace/tokens.json
TEMP_CACHE_FILE_PATH=$workspace/cache_temp.json
API_SOCKET_PATH=$workspace/api.sock
ADMIN_SOCKET_PATH=$workspace/admin.sock
SERVER_MEMORY_MB=512
SERVER_CPU_COUNT=2
LOG_NETWORK_REQUESTS=true
EOF
chmod 0600 "$workspace/.env" "$workspace/tokens.json"

"$binary" --version | grep -Eq '^jcb-api [0-9]+\.[0-9]+\.[0-9]+'
"$binary" --help > /dev/null
if cli check > /dev/null 2>&1; then fail 'check reported a healthy service before startup'; fi
start_service
cli check > /dev/null

# Existing data and tokens are usable without translation or credential replacement.
expect_code 200 http://localhost/v1/network/community/jcb/legacy
jq -e '.network[0].id == 1 and .network[0].status == 1' "$workspace/response.json" > /dev/null
expect_code 403 http://localhost/v1/network/unknown/
expect_code 400 http://localhost/v1/network/community/jcb/legacy/not-an-integer
expect_code 200 http://localhost/v1/network/community/jcb/legacy/99
jq -e '.network == []' "$workspace/response.json" > /dev/null

# A second service must not become a competing persistence owner.
if timeout 5 "$binary" --env-file "$workspace/.env" serve > "$workspace/duplicate.log" 2>&1; then
    fail 'a second service accepted ownership of the same state'
fi
api http://localhost/healthz | jq -e '.status == "ok"' > /dev/null

# Public documentation does not disclose internal routes or token schemes.
api http://localhost/ > "$workspace/index.html"
grep -qi '<html' "$workspace/index.html"
if grep -qi webhook "$workspace/index.html"; then fail 'HTML documents the webhook'; fi
api http://localhost/openapi.json > "$workspace/openapi.json"
api http://localhost/OpenAPI.json > "$workspace/openapi-alias.json"
cmp "$workspace/openapi.json" "$workspace/openapi-alias.json"
jq -e '.openapi | startswith("3.")' "$workspace/openapi.json" > /dev/null
jq -e '.paths | keys | length > 0' "$workspace/openapi.json" > /dev/null
if grep -qi webhook "$workspace/openapi.json"; then fail 'OpenAPI documents the webhook'; fi

payload='{"heartbeat":{"monitorID":99,"status":1,"msg":"Online"},"monitor":{"name":"New mirror","url":"https://example.invalid/new"}}'
expect_code 403 -X POST -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/community/jcb/new-target
expect_code 403 -X POST -H 'Authorization: incorrect' -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/community/jcb/new-target
expect_code 200 -X POST -H 'Authorization: legacy-smoke-token' -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/community/jcb/new-target
expect_code 200 http://localhost/v1/network/community/jcb/new-target/1
jq -e '.network | length == 1' "$workspace/response.json" > /dev/null

# Token administration is live, scoped to a system, and lists fingerprints only.
new_token=$(cli token create --system fresh)
[[ ${#new_token} -ge 32 ]] || fail 'generated token is unexpectedly short'
cli token list --system fresh > "$workspace/token-list.json"
fingerprint=$(printf '%s' "$new_token" | sha256sum | cut -d ' ' -f 1)
jq -e --arg fp "$fingerprint" '.fresh | any(.fingerprint == $fp)' "$workspace/token-list.json" > /dev/null
if grep -Fq -- "$new_token" "$workspace/token-list.json"; then fail 'token listing reveals its secret'; fi
expect_code 200 -X POST -H "Authorization: $new_token" -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/fresh/new-project/new-target
expect_code 200 http://localhost/v1/network/fresh/new-project/new-target
jq -e '.network[0].id == 99' "$workspace/response.json" > /dev/null
expect_code 403 -X POST -H "Authorization: $new_token" -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/community/jcb/new-target

# A compilation-sized burst runs only against this temporary test instance.
export JCB_SMOKE_SOCKET="$workspace/api.sock"
seq 1 100 | xargs -P 16 -I '{}' sh -c \
    'test "$(curl --silent --show-error --max-time 5 --unix-socket "$JCB_SMOKE_SOCKET" -o /dev/null -w "%{http_code}" http://localhost/v1/network/community/jcb/legacy)" = 200'

# Removal changes prepared responses immediately and survives restart.
cli network remove --system community --project jcb --target new-target --id 99 --yes > /dev/null
expect_code 403 http://localhost/v1/network/community/jcb/new-target
cli network remove --system fresh --yes > /dev/null
expect_code 403 http://localhost/v1/network/fresh/
cli token revoke --system fresh --fingerprint "$fingerprint" > /dev/null
expect_code 403 -X POST -H "Authorization: $new_token" -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/fresh/new-project/new-target

# Existing files can be replaced atomically and explicitly reloaded safely.
jq '.community += ["reloaded-smoke-token"]' "$workspace/tokens.json" > "$workspace/tokens.next"
mv "$workspace/tokens.next" "$workspace/tokens.json"
cli reload > /dev/null
expect_code 200 -X POST -H 'Authorization: reloaded-smoke-token' -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/community/jcb/reloaded
stop_service
start_service
expect_code 200 http://localhost/v1/network/community/jcb/reloaded
expect_code 403 http://localhost/v1/network/community/jcb/new-target
expect_code 403 http://localhost/v1/network/fresh/
stop_service
if grep -Fq -- "$new_token" "$workspace/service.log"; then fail 'logs contain a generated token'; fi
if grep -Fq 'legacy-smoke-token' "$workspace/service.log"; then fail 'logs contain a legacy token'; fi

# Exercise the exact checksum format used by deployment and release workflows.
cp "$binary" "$workspace/jcb-api-x86_64-unknown-linux-musl"
(
    cd "$workspace"
    sha256sum jcb-api-x86_64-unknown-linux-musl > SHA256SUMS
)
"$binary" release-verify --manifest "$workspace/SHA256SUMS" \
    --asset "$workspace/jcb-api-x86_64-unknown-linux-musl"
cp "$binary" "$workspace/jcb-api-unlisted"
if "$binary" release-verify --manifest "$workspace/SHA256SUMS" --asset "$workspace/jcb-api-unlisted"; then
    fail 'release asset missing from the checksum manifest was accepted'
fi
printf 'tampered' >> "$workspace/jcb-api-x86_64-unknown-linux-musl"
if "$binary" release-verify --manifest "$workspace/SHA256SUMS" \
    --asset "$workspace/jcb-api-x86_64-unknown-linux-musl"; then
    fail 'tampered release asset was accepted'
fi

printf 'API, CLI, migration, burst, restart, and release checksum smoke checks passed.\n'
