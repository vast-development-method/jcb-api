# Releases and automatic updates

GitHub Actions builds precompiled Linux executables and publishes a `SHA256SUMS`
file alongside them. The installer and updater download both over HTTPS from the
configured GitHub repository and verify the binary's checksum before executing
or installing it. GitHub is the trusted distribution source; the checksum checks
that the downloaded bytes match the release.

No signing keys, custom secrets, or GitHub environment setup are required.
Publishing uses the automatically provided `GITHUB_TOKEN`, with write permission
only in the publication job. Production servers download public releases without
credentials and do not compile source code.

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
executables and `SHA256SUMS`, generated using `sha256sum` after the builds pass.
The workflow checks both hashes and uploads all three files before publishing
the release.

The workflow can also publish SemVer prerelease tags, marked as prereleases on
GitHub. Only stable releases are eligible for automatic installation. A published release
is not overwritten. Use a new version for corrected published artifacts.

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

CI exercises both architecture builds, checksum verification, rejection of a
modified binary, and release-selection logic. Publishing runs when a version tag
is pushed, or when a backfill is requested after the workflow is on `main`.

## Manual verification

The native CLI can verify a previously downloaded release set:

```bash
/opt/jcb-api/current/jcb-api release-verify \
  --manifest SHA256SUMS \
  --asset jcb-api-x86_64-unknown-linux-musl
```

Choose the artifact matching the host architecture. This checks its hash against
the manifest without executing the downloaded program. For a first installation,
the bootstrap installer uses the system's `sha256sum` before running the binary.
If both architecture files were downloaded, the standard command also works:

```bash
sha256sum --check SHA256SUMS
```

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
