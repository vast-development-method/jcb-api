# Releases and update trust

Release artifacts are signed prebuilt executables. Build and signing operations
run in GitHub Actions or on a maintainer workstation. Production servers verify
and install binaries; they never compile source code.

## One-time maintainer setup

1. Protect `main` and the `v*` release tags. Restrict who can publish releases and
   modify the release workflow. Require review for workflow/deployment changes.
2. Create the GitHub Actions environment named `release`. Apply appropriate
   approval rules and restrict deployment refs. Pull-request validation must not
   receive production signing credentials.
3. Generate an Ed25519 signing key on a trusted workstation using the Rust CLI.
   Keep private keys outside the repository, even temporarily.

```bash
cargo build --locked --release
mkdir -p "$HOME/jcb-api-release-keys"
chmod 700 "$HOME/jcb-api-release-keys"
./target/release/jcb-api release-keygen \
  --private-key "$HOME/jcb-api-release-keys/release.private.pem" \
  --public-key "$HOME/jcb-api-release-keys/release-public.pem"
```

4. Store the complete private PKCS8 PEM as the `RELEASE_SIGNING_KEY` secret and
   the complete public SPKI PEM as the `RELEASE_PUBLIC_KEY` variable available to
   the `release` environment. GitHub's Settings → Environments → release screen
   can be used; do not paste the private key into an issue, commit, or workflow.
5. Back up the private key securely. Distribute the public key and its fingerprint
   through a trusted channel. Administrators provide that trusted public key to
   the installer. A key downloaded beside an unsigned binary proves nothing.

A public-key fingerprint can be compared independently with OpenSSL:

```bash
openssl pkey -pubin \
  -in "$HOME/jcb-api-release-keys/release-public.pem" -outform DER |
  openssl dgst -sha256
```

A compromised signing key requires a deliberate trust-key replacement on installed
servers. Automatic acceptance of arbitrary new keys would defeat signature
verification. The updater therefore does not silently rotate its own trust root.

## Publish a version

First update the package version in `Cargo.toml`, update `Cargo.lock`, and merge
the tested changes to `main`. The tag is derived from the package version rather
than hardcoded in scripts:

```bash
git switch main
git pull --ff-only
release_version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n 1)
test -n "$release_version"
git tag -a "v$release_version" -m "Release v$release_version"
git push origin "v$release_version"
```

The release workflow rejects a version mismatch. It builds both supported targets:

| Target | Artifact |
| --- | --- |
| `x86_64-unknown-linux-musl` | `jcb-api-x86_64-unknown-linux-musl` |
| `aarch64-unknown-linux-musl` | `jcb-api-aarch64-unknown-linux-musl` |

Native Linux runners build each target. The final release contains both raw
executables, `SHA256SUMS`, and `SHA256SUMS.sig`. The signature is the raw 64-byte
Ed25519 signature over the exact manifest bytes. Signatures authenticate the
manifest; SHA-256 entries bind each asset's name to its bytes.

The workflow can also publish SemVer prerelease tags, marked as prereleases on
GitHub. Only stable releases are eligible for automatic installation. A published release
is not overwritten. Use a new version for corrected published artifacts. The
workflow fails closed when signing material is absent or does not match.

## Backfill an existing tag

Use Actions → Release → Run workflow on `main` and enter the existing tag in the
`tag` input. This supports a tag whose release was never published or whose build
failed before publication. An unpublished draft can be resumed with its assets
replaced; a published release is immutable to this workflow. The tag must name a
commit reachable from `main`, and
its package version must match. It does not allow publishing arbitrary branch
artifacts or silently overwriting an existing release.

Publication uses GitHub's legacy semantic-version policy for selecting the
latest stable release, so publishing a backfill does not automatically promote
it merely because it was the most recent workflow run.

CI exercises builds and signature checks with temporary test keys. Those keys are
never trusted by production installers. PR checks exercise packaging, signature
interoperability, and release-selection logic without production credentials.
Publishing with the real signing key is a separate run after maintainer setup.

## Manual verification

The native CLI can verify a previously downloaded release set:

```bash
sudo /opt/jcb-api/current/jcb-api release-verify \
  --manifest SHA256SUMS \
  --signature SHA256SUMS.sig \
  --public-key /etc/jcb-api/release-public.pem \
  --asset jcb-api-x86_64-unknown-linux-musl
```

Choose the artifact matching the host architecture. This checks both signature
and digest; it does not execute the downloaded program. The bootstrap installer
uses OpenSSL for the equivalent first-install verification, before a trusted Rust
binary is available. Downloaded binaries must never be used to verify themselves
before their signatures have been checked.

## Update operations

With the default installation:

```bash
sudo /opt/jcb-api/current/jcb-api update --check
sudo systemctl start jcb-api-update.service
sudo journalctl -u jcb-api-update.service -f
```

The updater prevents concurrent activation, skips the current version, and refuses
downgrades. It verifies the asset, checks the binary's version, creates a new
release directory, swaps `current`, and restarts the service. Health validation
must report the new version, not merely return HTTP 200 from an old process.
On failure the previous symlink is restored and the previous service restarted.

For a particular newer stable release, use `update --release vX.Y.Z` with the
actual published version. That does not bypass the no-downgrade rule. The saved
systemd updater unit contains explicit path/socket/repository arguments for
non-default installations.

A failed verified release directory is retained for investigation. A later timer
run can retry it automatically: the updater freshly downloads and verifies the
release, then reuses the existing binary only if its protected file matches the
verified digest exactly. Different contents under the same version are refused
for inspection. Do not remove the directory referenced by `current` or the
previous working release.

### Manual rollback

Stop the automatic update timer first. Inspect retained release directories and
choose the exact prior version. For example, the following prompts avoid silently
selecting an arbitrary historical binary:

```bash
(
set -eu
sudo systemctl stop jcb-api-update.timer
ls -1 /opt/jcb-api/releases
read -r -p 'Retained release tag to restore: ' rollback_tag
[[ "$rollback_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
test -x "/opt/jcb-api/releases/$rollback_tag/jcb-api"
sudo systemctl stop jcb-api.service
sudo ln -s "releases/$rollback_tag" /opt/jcb-api/.current-rollback
sudo mv -Tf /opt/jcb-api/.current-rollback /opt/jcb-api/current
sudo systemctl start jcb-api.service
sudo curl --fail --unix-socket /run/jcb-api/api.sock http://localhost/healthz
)
```

Check state-format compatibility before running an older binary. This operation
changes code only; it does not roll back cache or token data. Keep protected data
backups separately. Re-enable the update timer after addressing the failed
release; otherwise it may select the same newer release again.
