#!/usr/bin/env bash
# Resolve a release without building, signing, changing Git, or publishing it.
set -euo pipefail

release_tag=
event_name=push
workflow_ref=
main_ref=origin/main
repository=.
while (($#)); do
    case "$1" in
        --tag|--event|--workflow-ref|--main-ref|--repository)
            (($# >= 2)) || { printf 'Missing value for %s\n' "$1" >&2; exit 2; }
            case "$1" in
                --tag) release_tag=$2 ;;
                --event) event_name=$2 ;;
                --workflow-ref) workflow_ref=$2 ;;
                --main-ref) main_ref=$2 ;;
                --repository) repository=$2 ;;
            esac
            shift 2
            ;;
        *) printf 'Unknown release-plan argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done
die() { printf '%s\n' "$*" >&2; exit 1; }

case "$event_name" in
    push) ;;
    workflow_dispatch)
        [[ "$workflow_ref" == refs/heads/main ]] || die 'Backfill must be dispatched from the main branch.'
        ;;
    *) die 'Unsupported release event.' ;;
esac
[[ "$main_ref" =~ ^[A-Za-z0-9][A-Za-z0-9_./-]*$ ]] || die 'Invalid main reference.'
[[ "$release_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]] ||
    die 'Release tag must be vMAJOR.MINOR.PATCH with an optional SemVer prerelease.'
prerelease=false
if [[ "$release_tag" == *-* ]]; then
    prerelease=true
    IFS=. read -r -a identifiers <<< "${release_tag#*-}"
    for identifier in "${identifiers[@]}"; do
        if [[ "$identifier" =~ ^[0-9]+$ && "$identifier" == 0?* ]]; then
            die 'Numeric prerelease identifiers cannot have leading zeroes.'
        fi
    done
fi

release_sha=$(git -C "$repository" rev-parse --verify "refs/tags/$release_tag^{commit}")
git -C "$repository" merge-base --is-ancestor "$release_sha" "$main_ref" ||
    die 'Release tag is not on main branch history.'
manifest=$(git -C "$repository" show "$release_sha:Cargo.toml")
# The crate has an explicit package.version. The package build separately checks
# Cargo's own parsed metadata, so this early check cannot replace build validation.
package_version=$(awk '
    /^[[:space:]]*\[/ { in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*(#.*)?$/); next }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
        value = $0
        sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", value)
        sub(/"[[:space:]]*(#.*)?$/, "", value)
        print value
        exit
    }
' <<< "$manifest")
[[ -n "$package_version" && "$release_tag" == "v$package_version" ]] ||
    die 'Tag does not match Cargo.toml package.version at the tagged commit.'
printf 'tag=%s\nsha=%s\nversion=%s\nprerelease=%s\n' "$release_tag" "$release_sha" "$package_version" "$prerelease"
