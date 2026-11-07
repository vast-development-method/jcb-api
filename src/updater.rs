//! Release distribution. The serving process never calls this module's updater.
//!
//! The binary and SHA256SUMS are downloaded from the configured GitHub release
//! over HTTPS. The checksum is verified before the candidate binary is executed.

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use fs2::FileExt;
use reqwest::{blocking::Client, redirect::Policy, Url};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_API_RESPONSE: u64 = 1024 * 1024;
const MAX_BINARY: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    #[arg(long, default_value = "vast-development-method/jcb-api")]
    pub repository: String,
    #[arg(long, default_value = "/opt/jcb-api")]
    pub install_dir: PathBuf,
    #[arg(long, default_value = "/run/jcb-api/api.sock")]
    pub socket: PathBuf,
    #[arg(long, default_value = "jcb-api.service")]
    pub service: String,
    /// Report availability without downloading or activating a binary.
    #[arg(long)]
    pub check: bool,
    /// Published stable tag to install; downgrades are still refused.
    #[arg(long)]
    pub release: Option<String>,
    /// Override native target selection (normally unnecessary).
    #[arg(long)]
    pub target: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    AlreadyCurrent {
        version: Version,
    },
    Available {
        current: Version,
        available: Version,
    },
    Updated {
        previous: Version,
        current: Version,
    },
}

impl fmt::Display for UpdateOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyCurrent { version } => write!(f, "Already current: {version}"),
            Self::Available { current, available } => {
                write!(f, "Update available: {current} -> {available}")
            }
            Self::Updated { previous, current } => {
                write!(f, "Updated successfully: {previous} -> {current}")
            }
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Verify the asset's exact checksum against its release manifest.
pub fn verify(manifest: &Path, asset: &Path) -> Result<()> {
    let input = read_bounded(manifest, MAX_MANIFEST)?;
    let entries = parse_manifest(&input)?;
    let name = asset
        .file_name()
        .and_then(|s| s.to_str())
        .context("Asset requires a UTF-8 filename")?;
    let expected = entries
        .get(name)
        .context("Asset is absent from the release manifest")?;
    let actual = digest_file(asset)?;
    if actual != *expected {
        bail!("Release asset checksum mismatch for {name}");
    }
    Ok(())
}

/// Run outside the async HTTP executor. Activation is serialized across processes.
pub fn run(args: UpdateArgs) -> Result<UpdateOutcome> {
    validate_repository(&args.repository)?;
    validate_service(&args.service)?;
    let target = args.target.clone().unwrap_or_else(native_target);
    validate_target(&target)?;
    ensure_root_protected(&args.install_dir, true)?;
    let current = installed_version(&args.install_dir)?;
    let client = http_client()?;
    let release = fetch_release(&client, &args.repository, args.release.as_deref())?;
    if release.draft || release.prerelease {
        bail!("Only published stable releases may be installed");
    }
    let candidate = stable_version(&release.tag_name)?;
    ensure_not_downgrade(&current, &candidate)?;
    if candidate == current {
        return Ok(UpdateOutcome::AlreadyCurrent { version: current });
    }
    if args.check {
        return Ok(UpdateOutcome::Available {
            current,
            available: candidate,
        });
    }

    // Runtime account must not be able to replace the installation.
    ensure_root_protected(&args.install_dir, true)?;
    let lock_path = args.install_dir.join(".update.lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("Update lock must not be a symlink");
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)?;
    lock.try_lock_exclusive()
        .context("Another update is already running")?;
    // A simultaneous updater may have completed while the release was fetched.
    let current = installed_version(&args.install_dir)?;
    ensure_not_downgrade(&current, &candidate)?;
    if candidate == current {
        return Ok(UpdateOutcome::AlreadyCurrent { version: current });
    }
    let current_link = args.install_dir.join("current");
    let previous =
        fs::read_link(&current_link).context("Installation requires a current release symlink")?;
    validate_current_link(&previous)?;
    let releases = args.install_dir.join("releases");
    fs::create_dir_all(&releases)?;
    ensure_root_protected(&releases, true)?;
    let stage = Stage::create(&releases)?;
    let asset_name = format!("jcb-api-{target}");
    for (name, limit) in [
        ("SHA256SUMS", MAX_MANIFEST),
        (asset_name.as_str(), MAX_BINARY),
    ] {
        let asset = unique_asset(&release, name)?;
        if asset.size > limit {
            bail!("Release asset {name} exceeds its size limit");
        }
        download(
            &client,
            &asset.browser_download_url,
            &stage.path.join(name),
            limit,
        )?;
    }
    let binary = stage.path.join(&asset_name);
    verify(&stage.path.join("SHA256SUMS"), &binary)?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
    if binary_version(&binary)? != candidate {
        bail!("Binary version does not match the GitHub release tag");
    }
    File::open(&binary)?.sync_all()?;
    fs::rename(&binary, stage.path.join("jcb-api"))?;
    sync_directory(&stage.path)?;
    let release_dir = releases.join(format!("v{candidate}"));
    if release_dir.exists() {
        // A transient health failure may have left this version staged.
        // Reuse only an exact, protected match to the freshly verified download.
        ensure_root_protected(&release_dir, true)?;
        let existing = release_dir.join("jcb-api");
        ensure_root_protected(&existing, false)?;
        if digest_file(&existing)? != digest_file(&stage.path.join("jcb-api"))? {
            bail!("Existing release {} differs from the verified download; inspect it before proceeding", release_dir.display());
        }
    } else {
        fs::rename(&stage.path, &release_dir)?;
        sync_directory(&releases)?;
    }
    let desired = PathBuf::from("releases").join(format!("v{candidate}"));
    let activation = activate_with_rollback(
        &args.install_dir,
        &previous,
        &desired,
        &current,
        &candidate,
        |version| restart_and_check(&args.service, &args.socket, version),
    );
    if let Err(error) = activation {
        // Preserve the failed verified release; the next timer run can retry it.
        return Err(error.context(format!(
            "Verified release retained at {}; the next update attempt will verify and retry it",
            release_dir.display()
        )));
    }
    Ok(UpdateOutcome::Updated {
        previous: current,
        current: candidate,
    })
}

fn activate_with_rollback(
    install: &Path,
    previous: &Path,
    desired: &Path,
    old_version: &Version,
    new_version: &Version,
    mut restart: impl FnMut(&Version) -> Result<()>,
) -> Result<()> {
    // A directory fsync can fail after rename has already replaced the link.
    // Roll back on either publication or startup failure, not just failed health.
    let activation = switch_link(install, desired).and_then(|()| restart(new_version));
    if let Err(activation) = activation {
        let rollback = switch_link(install, previous).and_then(|()| restart(old_version));
        return match rollback {
            Ok(()) => Err(anyhow!(
                "New release failed health checks; previous release restored: {activation:#}"
            )),
            Err(error) => Err(anyhow!(
                "New release failed: {activation:#}; rollback also failed: {error:#}"
            )),
        };
    }
    Ok(())
}

fn installed_version(install: &Path) -> Result<Version> {
    let current = install.join("current");
    let link =
        fs::read_link(&current).context("No installed current release; run the installer first")?;
    validate_current_link(&link)?;
    let directory = install.join(&link);
    ensure_root_protected(&directory, true)?;
    let binary = directory.join("jcb-api");
    ensure_root_protected(&binary, false)?;
    binary_version(&binary)
}

fn binary_version(binary: &Path) -> Result<Version> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Binary version check timed out");
        }
        thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        bail!("Binary version check failed");
    }
    let mut output = String::new();
    child
        .stdout
        .take()
        .context("Missing version output")?
        .take(1024)
        .read_to_string(&mut output)?;
    let version = output
        .trim()
        .strip_prefix("jcb-api ")
        .context("Unexpected binary version output")?;
    stable_version(version)
}

fn stable_version(input: &str) -> Result<Version> {
    let version = Version::parse(input.strip_prefix('v').unwrap_or(input))
        .context("Invalid release version")?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("Only stable major.minor.patch versions are supported");
    }
    Ok(version)
}

fn ensure_not_downgrade(current: &Version, candidate: &Version) -> Result<()> {
    if candidate < current {
        bail!("Refusing downgrade from {current} to {candidate}");
    }
    Ok(())
}

fn native_target() -> String {
    let architecture = std::env::consts::ARCH;
    let environment = if cfg!(target_env = "musl") {
        "musl"
    } else {
        "gnu"
    };
    format!("{architecture}-unknown-linux-{environment}")
}

fn validate_target(target: &str) -> Result<()> {
    if !matches!(
        target,
        "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-unknown-linux-musl"
            | "aarch64-unknown-linux-musl"
    ) {
        bail!("Unsupported release target {target}");
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    let components: Vec<_> = repository.split('/').collect();
    if components.len() != 2
        || components.iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        bail!("Repository must be a GitHub owner/name pair");
    }
    Ok(())
}

fn validate_service(service: &str) -> Result<()> {
    if service.is_empty()
        || service.starts_with('-')
        || !service
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.@".contains(&c))
    {
        bail!("Invalid systemd service name");
    }
    Ok(())
}

fn validate_current_link(link: &Path) -> Result<()> {
    let components: Vec<_> = link.components().collect();
    if components.len() != 2 || components[0].as_os_str() != "releases" {
        bail!("current must point to a relative releases/vMAJOR.MINOR.PATCH directory");
    }
    stable_version(
        components[1]
            .as_os_str()
            .to_str()
            .context("Invalid release link")?,
    )?;
    Ok(())
}

fn trusted_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        )
}

fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("jcb-api-updater/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !trusted_url(attempt.url()) {
                attempt.error("Untrusted release redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

fn fetch_release(client: &Client, repository: &str, tag: Option<&str>) -> Result<Release> {
    let mut url = Url::parse("https://api.github.com/")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| anyhow!("Invalid GitHub URL"))?;
        path.push("repos");
        for part in repository.split('/') {
            path.push(part);
        }
        path.push("releases");
        match tag {
            Some(tag) => {
                stable_version(tag)?;
                path.push("tags").push(tag);
            }
            None => {
                path.push("latest");
            }
        }
    }
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()?;
    let bytes = limited_reader(response, MAX_API_RESPONSE)?;
    serde_json::from_slice(&bytes).context("Invalid GitHub release response")
}

fn unique_asset<'a>(release: &'a Release, name: &str) -> Result<&'a Asset> {
    let mut found = release.assets.iter().filter(|a| a.name == name);
    let asset = found
        .next()
        .with_context(|| format!("Missing release asset {name}"))?;
    if found.next().is_some() {
        bail!("Duplicate release asset {name}");
    }
    Ok(asset)
}

fn download(client: &Client, source: &str, destination: &Path, limit: u64) -> Result<()> {
    let url = Url::parse(source)?;
    if !trusted_url(&url) {
        bail!("Untrusted release asset URL");
    }
    let response = client.get(url).send()?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("Release download is too large");
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(destination)?;
    let copied = std::io::copy(&mut response.take(limit + 1), &mut file)?;
    if copied > limit {
        bail!("Release download exceeded its size limit");
    }
    file.sync_all()?;
    Ok(())
}

fn parse_manifest(bytes: &[u8]) -> Result<HashMap<String, [u8; 32]>> {
    let text = std::str::from_utf8(bytes)?;
    let mut entries = HashMap::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let (hash, name) = line.split_once("  ").context("Invalid SHA256SUMS line")?;
        if hash.len() != 64
            || name.is_empty()
            || name.len() > 200
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            || matches!(name, "." | "..")
        {
            bail!("Manifest entries require a SHA-256 digest and a safe basename");
        }
        let decoded = hex::decode(hash).context("Invalid manifest digest")?;
        let digest: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow!("Invalid digest length"))?;
        if entries.insert(name.to_owned(), digest).is_some() {
            bail!("Duplicate manifest entry");
        }
    }
    if entries.is_empty() {
        bail!("Manifest is empty");
    }
    Ok(entries)
}

fn digest_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() > MAX_BINARY {
        bail!("Release binary is too large");
    }
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().into())
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    limited_reader(File::open(path)?, limit)
}

fn limited_reader(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Input exceeds maximum permitted size");
    }
    Ok(bytes)
}

fn ensure_root_protected(path: &Path, directory: bool) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("Protected installation paths must be absolute and contain no parent traversal");
    }
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("Inspecting {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o022 != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        bail!(
            "{} must be root-owned, not a symlink, and not group/world-writable",
            path.display()
        );
    }
    for parent in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.permissions().mode() & 0o022 != 0
        {
            bail!("Parent directory {} must be root-owned, not a symlink, and not group/world-writable", parent.display());
        }
    }
    Ok(())
}

fn switch_link(install: &Path, target: &Path) -> Result<()> {
    validate_current_link(target)?;
    let next = install.join(format!(
        ".current-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    symlink(target, &next)?;
    if let Err(error) = fs::rename(&next, install.join("current")) {
        let _ = fs::remove_file(&next);
        return Err(error.into());
    }
    sync_directory(install)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all().map_err(Into::into)
}

fn restart_and_check(service: &str, socket: &Path, expected: &Version) -> Result<()> {
    let status = Command::new("systemctl")
        .args(["--no-block", "restart", service])
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("systemctl restart failed for {service}");
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if check_health(socket, expected).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    bail!("Release {expected} did not become healthy within 30 seconds")
}

fn check_health(socket: &Path, expected: &Version) -> Result<()> {
    // A total async deadline covers connect as well as slow/trickling responses;
    // SO_RCVTIMEO alone would only bound each individual blocking read.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let response = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut connection = tokio::net::UnixStream::connect(socket).await?;
            connection
                .write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await?;
            let mut response = Vec::new();
            connection
                .take(16 * 1024 + 1)
                .read_to_end(&mut response)
                .await?;
            if response.len() > 16 * 1024 {
                bail!("Health response exceeds size limit");
            }
            Ok::<_, anyhow::Error>(response)
        })
        .await
        .context("Health request timed out")?
    })?;
    let response = std::str::from_utf8(&response)?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .context("Invalid health response")?;
    if !headers
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("HTTP/1.0 200 ") || line.starts_with("HTTP/1.1 200 "))
    {
        bail!("Health check returned an error");
    }
    let data: serde_json::Value = serde_json::from_str(body)?;
    if data.get("status").and_then(|v| v.as_str()) != Some("ok") {
        bail!("Service reports an unhealthy status");
    }
    if data.get("version").and_then(|v| v.as_str()) != Some(expected.to_string().as_str()) {
        bail!("Healthy process has a different version");
    }
    Ok(())
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

struct Stage {
    path: PathBuf,
}
impl Stage {
    fn create(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(".stage-{}-{}", std::process::id(), unique_suffix()));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(Self { path })
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn checksum_verification_accepts_matching_asset_and_rejects_changes() -> Result<()> {
        let dir = tempdir()?;
        let asset = dir.path().join("jcb-api-x86_64-unknown-linux-gnu");
        fs::write(&asset, b"test release content")?;
        let manifest = dir.path().join("SHA256SUMS");
        let original = format!(
            "{}  {}\n",
            hex::encode(digest_file(&asset)?),
            asset.file_name().unwrap().to_str().unwrap()
        );
        fs::write(&manifest, &original)?;
        verify(&manifest, &asset)?;
        fs::write(&asset, b"modified")?;
        assert!(verify(&manifest, &asset).is_err());
        fs::write(&asset, b"test release content")?;
        fs::write(
            &manifest,
            format!("{}  jcb-api-x86_64-unknown-linux-gnu\n", "0".repeat(64)),
        )?;
        assert!(verify(&manifest, &asset).is_err());
        fs::write(&manifest, &original)?;
        let absent = dir.path().join("unlisted-binary");
        fs::write(&absent, b"test release content")?;
        assert!(verify(&manifest, &absent).is_err());
        fs::write(&manifest, format!("{original}{original}"))?;
        assert!(verify(&manifest, &asset).is_err());
        fs::write(&manifest, b"invalid manifest")?;
        assert!(verify(&manifest, &asset).is_err());
        Ok(())
    }

    #[test]
    fn manifest_rejects_ambiguous_paths_and_missing_assets() {
        let hash = "a".repeat(64);
        for name in [
            "../binary",
            "/binary",
            "binary/name",
            "..",
            "name with spaces",
        ] {
            assert!(parse_manifest(format!("{hash}  {name}\n").as_bytes()).is_err());
        }
        assert!(parse_manifest(format!("{hash}  binary\n{hash}  binary\n").as_bytes()).is_err());
        assert!(parse_manifest(b"").is_err());
        assert!(parse_manifest(b"broken  binary\n").is_err());
    }

    #[test]
    fn versions_and_download_origins_are_restricted() -> Result<()> {
        assert!(stable_version("v1.2.3-rc.1").is_err());
        assert!(stable_version("v1.2.3+other").is_err());
        assert!(ensure_not_downgrade(&Version::new(1, 2, 3), &Version::new(1, 2, 2)).is_err());
        ensure_not_downgrade(&Version::new(1, 2, 3), &Version::new(1, 2, 4))?;
        assert!(trusted_url(&Url::parse(
            "https://release-assets.githubusercontent.com/file"
        )?));
        for url in [
            "http://github.com/file",
            "https://github.com.evil.test/file",
            "https://user@github.com/file",
            "https://github.com:8443/file",
            "file:///etc/passwd",
        ] {
            assert!(!trusted_url(&Url::parse(url)?));
        }
        assert!(validate_repository("owner/../repo").is_err());
        assert!(validate_service("--help").is_err());
        assert!(validate_current_link(Path::new("../../evil")).is_err());
        Ok(())
    }

    #[test]
    fn failed_activation_restores_previous_link_and_checks_old_version() -> Result<()> {
        let dir = tempdir()?;
        let old = Path::new("releases/v1.0.0");
        let new = Path::new("releases/v1.1.0");
        symlink(old, dir.path().join("current"))?;
        let mut calls = Vec::new();
        let result = activate_with_rollback(
            dir.path(),
            old,
            new,
            &Version::new(1, 0, 0),
            &Version::new(1, 1, 0),
            |version| {
                calls.push(version.clone());
                if *version == Version::new(1, 1, 0) {
                    bail!("simulated failed health check");
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read_link(dir.path().join("current"))?, old);
        assert_eq!(calls, [Version::new(1, 1, 0), Version::new(1, 0, 0)]);
        Ok(())
    }

    #[test]
    fn successful_activation_keeps_new_release() -> Result<()> {
        let dir = tempdir()?;
        let old = Path::new("releases/v1.0.0");
        let new = Path::new("releases/v1.1.0");
        symlink(old, dir.path().join("current"))?;
        activate_with_rollback(
            dir.path(),
            old,
            new,
            &Version::new(1, 0, 0),
            &Version::new(1, 1, 0),
            |_| Ok(()),
        )?;
        assert_eq!(fs::read_link(dir.path().join("current"))?, new);
        Ok(())
    }

    #[test]
    fn rollback_failure_is_not_reported_as_success() -> Result<()> {
        let dir = tempdir()?;
        let old = Path::new("releases/v1.0.0");
        let new = Path::new("releases/v1.1.0");
        symlink(old, dir.path().join("current"))?;
        let result = activate_with_rollback(
            dir.path(),
            old,
            new,
            &Version::new(1, 0, 0),
            &Version::new(1, 1, 0),
            |_| bail!("restart failed"),
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("rollback also failed"));
        assert_eq!(fs::read_link(dir.path().join("current"))?, old);
        Ok(())
    }

    #[test]
    fn bounded_inputs_reject_extra_bytes() {
        assert!(limited_reader(&b"four"[..], 3).is_err());
        assert_eq!(limited_reader(&b"four"[..], 4).unwrap(), b"four");
    }

    #[test]
    fn health_check_rejects_old_process_version() -> Result<()> {
        use std::os::unix::net::UnixListener;
        let dir = tempdir()?;
        let path = dir.path().join("health.sock");
        let listener = UnixListener::bind(&path)?;
        let thread = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0; 512];
            let _ = connection.read(&mut request).unwrap();
            connection.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\",\"version\":\"1.0.0\"}").unwrap();
        });
        assert!(check_health(&path, &Version::new(1, 1, 0)).is_err());
        thread.join().unwrap();
        Ok(())
    }

    #[test]
    fn health_check_accepts_the_expected_healthy_version() -> Result<()> {
        use std::os::unix::net::UnixListener;
        let dir = tempdir()?;
        let path = dir.path().join("health.sock");
        let listener = UnixListener::bind(&path)?;
        let thread = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0; 512];
            let _ = connection.read(&mut request).unwrap();
            connection.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\",\"version\":\"1.1.0\"}").unwrap();
        });
        check_health(&path, &Version::new(1, 1, 0))?;
        thread.join().unwrap();
        Ok(())
    }
}
