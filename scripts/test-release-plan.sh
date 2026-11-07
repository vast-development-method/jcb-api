#!/usr/bin/env bash
# Isolated Git fixtures exercise release/backfill validation without publishing.
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
workspace=$(mktemp -d)
trap 'rm -rf -- "$workspace"' EXIT
git init -q --initial-branch=main "$workspace/repo"
git -C "$workspace/repo" config user.name 'Release Fixture'
git -C "$workspace/repo" config user.email 'release-fixture@example.invalid'
write_version() {
    cat > "$workspace/repo/Cargo.toml" <<EOF
[package]
name = "jcb-api"
version = "$1"
edition = "2021"

[dependencies]
EOF
    git -C "$workspace/repo" add Cargo.toml
    git -C "$workspace/repo" commit -qm "Fixture $1"
}
plan() { bash "$script_dir/release-plan.sh" --repository "$workspace/repo" --main-ref main "$@"; }
reject() {
    if plan "$@" > "$workspace/out" 2> "$workspace/err"; then
        printf 'Invalid release plan accepted: %s\n' "$*" >&2
        exit 1
    fi
}

write_version 1.2.3
git -C "$workspace/repo" tag v1.2.3
plan --tag v1.2.3 > "$workspace/plan"
grep -qx 'version=1.2.3' "$workspace/plan"
grep -qx 'prerelease=false' "$workspace/plan"
plan --tag v1.2.3 --event workflow_dispatch --workflow-ref refs/heads/main > /dev/null
reject --tag v1.2.3 --event workflow_dispatch --workflow-ref refs/heads/feature/untrusted
reject --tag v1.2.3 --event pull_request
reject --tag v01.2.3
reject --tag v1.2.3-01
reject --tag 'v1.2.3/../../main'
reject --tag v9.9.9
git -C "$workspace/repo" tag v1.2.4
reject --tag v1.2.4

write_version 1.3.0-rc.1
git -C "$workspace/repo" tag -am 'Annotated fixture' v1.3.0-rc.1
plan --tag v1.3.0-rc.1 > "$workspace/plan"
grep -qx 'prerelease=true' "$workspace/plan"
[[ $(sed -n 's/^sha=//p' "$workspace/plan") == "$(git -C "$workspace/repo" rev-parse HEAD)" ]]
plan --tag v1.2.3 --event workflow_dispatch --workflow-ref refs/heads/main > /dev/null

git -C "$workspace/repo" checkout -qb unrelated
write_version 2.0.0
git -C "$workspace/repo" tag v2.0.0
reject --tag v2.0.0
git -C "$workspace/repo" checkout -q main
git -C "$workspace/repo" rm -q Cargo.toml
git -C "$workspace/repo" commit -qm 'Remove manifest fixture'
git -C "$workspace/repo" tag v3.0.0
reject --tag v3.0.0

printf 'Release tags, package versions, ancestry, annotated tags, and backfill guards passed.\n'
