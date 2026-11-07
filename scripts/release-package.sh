#!/usr/bin/env bash
# Build one portable release artifact. Run on its native Linux architecture.
set -euo pipefail

release_target=${1:?Usage: release-package.sh TARGET [OUTPUT_DIRECTORY]}
output_dir=${2:-dist}
case "$release_target" in
    x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
    *) printf 'Unsupported release target: %s\n' "$release_target" >&2; exit 2 ;;
esac
case "$release_target:$(uname -m)" in
    x86_64-unknown-linux-musl:x86_64|aarch64-unknown-linux-musl:aarch64|aarch64-unknown-linux-musl:arm64) ;;
    *) printf 'Release packages must be built on their native architecture.\n' >&2; exit 2 ;;
esac

package_version=$(cargo metadata --locked --no-deps --format-version 1 |
    jq -er '.packages[] | select(.name == "jcb-api") | .version')
if [[ -n ${RELEASE_TAG:-} && "$RELEASE_TAG" != "v$package_version" ]]; then
    printf 'Tag %s does not match Cargo package version %s\n' "$RELEASE_TAG" "$package_version" >&2
    exit 1
fi

rustup target add "$release_target"
target_key=${release_target//-/_}
env "CC_${target_key}=musl-gcc" "AR_${target_key}=ar" \
    "CARGO_TARGET_${target_key^^}_LINKER=musl-gcc" \
    cargo build --locked --release --target "$release_target"
mkdir -p -- "$output_dir"
install -m 0755 "target/$release_target/release/jcb-api" "$output_dir/jcb-api-$release_target"
actual_version=$("$output_dir/jcb-api-$release_target" --version)
if [[ "$actual_version" != "jcb-api $package_version" ]]; then
    printf 'Packaged binary has unexpected version: %s\n' "$actual_version" >&2
    exit 1
fi
printf 'Packaged %s (%s)\n' "$release_target" "$package_version"
