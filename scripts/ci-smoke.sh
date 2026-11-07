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
        …1561 tokens truncated…n: reloaded-smoke-token' -H 'Content-Type: application/json' --data "$payload" \
    http://localhost/v1/webhook/community/jcb/reloaded
stop_service
start_service
expect_code 200 http://localhost/v1/network/community/jcb/reloaded
expect_code 403 http://localhost/v1/network/community/jcb/new-target
expect_code 403 http://localhost/v1/network/fresh/
stop_service
if grep -Fq -- "$new_token" "$workspace/service.log"; then fail 'logs contain a generated token'; fi
if grep -Fq 'legacy-smoke-token' "$workspace/service.log"; then fail 'logs contain a legacy token'; fi

# Exercise the exact signature format used by deployment and release workflows.
"$binary" release-keygen --private-key "$workspace/private.pem" --public-key "$workspace/public.pem"
cp "$binary" "$workspace/jcb-api-x86_64-unknown-linux-musl"
(
    cd "$workspace"
    sha256sum jcb-api-x86_64-unknown-linux-musl > SHA256SUMS
)
"$binary" release-sign --manifest "$workspace/SHA256SUMS" --private-key "$workspace/private.pem" --signature "$workspace/SHA256SUMS.sig"
"$binary" release-verify --manifest "$workspace/SHA256SUMS" --signature "$workspace/SHA256SUMS.sig" \
    --public-key "$workspace/public.pem" --asset "$workspace/jcb-api-x86_64-unknown-linux-musl"
openssl pkeyutl -verify -pubin -rawin -inkey "$workspace/public.pem" \
    -in "$workspace/SHA256SUMS" -sigfile "$workspace/SHA256SUMS.sig"
openssl pkeyutl -sign -rawin -inkey "$workspace/private.pem" -in "$workspace/SHA256SUMS" -out "$workspace/openssl.sig"
"$binary" release-verify --manifest "$workspace/SHA256SUMS" --signature "$workspace/openssl.sig" \
    --public-key "$workspace/public.pem" --asset "$workspace/jcb-api-x86_64-unknown-linux-musl"
printf 'tampered' >> "$workspace/jcb-api-x86_64-unknown-linux-musl"
if "$binary" release-verify --manifest "$workspace/SHA256SUMS" --signature "$workspace/SHA256SUMS.sig" \
    --public-key "$workspace/public.pem" --asset "$workspace/jcb-api-x86_64-unknown-linux-musl"; then
    fail 'tampered release asset was accepted'
fi

printf 'API, CLI, migration, burst, restart, and signature smoke checks passed.\n'
