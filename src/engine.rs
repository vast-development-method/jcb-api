//! The API's single authoritative state and pre-encoded public responses.
//!
//! Readers never parse files, serialize JSON, or take the mutation lock. Writers
//! prepare a complete candidate, persist it, and atomically publish its snapshot.
//! The files retain the original API's schema for import/export compatibility.

use anyhow::{anyhow, bail, Context, Result};
use arc_swap::ArcSwap;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;

const MAX_SEGMENT_BYTES: usize = 256;
const MAX_FIELD_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 4096;
const MAX_TOKENS_PER_SYSTEM: usize = 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

type Tokens = BTreeMap<String, Vec<String>>;
type TokenDigests = BTreeMap<String, Vec<[u8; 32]>>;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct Catalog {
    systems: Vec<System>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct System {
    system: String,
    projects: Vec<Project>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Project {
    project: String,
    targets: Vec<Target>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Target {
    target: String,
    network: Vec<Network>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Network {
    id: Value,
    name: String,
    url: String,
    status: i64,
}

struct Prepared {
    body: Bytes,
    systems: HashMap<String, PreparedSystem>,
}

struct PreparedSystem {
    body: Bytes,
    projects: HashMap<String, PreparedProject>,
}

struct PreparedProject {
    body: Bytes,
    targets: HashMap<String, PreparedTarget>,
}

struct PreparedTarget {
    body: Bytes,
    statuses: HashMap<i64, Bytes>,
    empty: Bytes,
}

struct Snapshot {
    catalog: Catalog,
    tokens: Tokens,
    token_digests: TokenDigests,
    prepared: Prepared,
    cache_stamp: Option<FileStamp>,
    tokens_stamp: Option<FileStamp>,
}

/// Metadata comparison makes the portable reload poll two stat calls when
/// nothing changed. Inodes catch atomic replacement; nanosecond modification
/// and change times catch in-place changes without reading JSON on every poll.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileStamp {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path).context("cannot inspect state file")?;
        if !metadata.file_type().is_file() {
            bail!("state path must be a regular file");
        }
        Ok(Self::from_metadata(&metadata))
    }
}

struct Inner {
    snapshot: ArcSwap<Snapshot>,
    writer: Arc<Mutex<()>>,
    cache_path: PathBuf,
    tokens_path: PathBuf,
    max_state_bytes: usize,
}

/// Cloneable handle to one state owner. The caller must also hold an exclusive
/// process lock for these files throughout the service's lifetime.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupError {
    SystemNotFound,
    ProjectNotFound,
    TargetNotFound,
    InvalidPath,
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SystemNotFound => "System not found",
            Self::ProjectNotFound => "Project not found",
            Self::TargetNotFound => "Target not found",
            Self::InvalidPath => "Invalid network path",
        })
    }
}

impl std::error::Error for LookupError {}

#[derive(Clone, Copy)]
enum FileKind {
    Catalog,
    Tokens,
}

impl Engine {
    /// Load existing legacy JSON files, or initialize an empty installation.
    /// This is intentionally synchronous and belongs in startup, not a handler.
    pub fn open(cache_path: PathBuf, tokens_path: PathBuf, max_state_bytes: usize) -> Result<Self> {
        if max_state_bytes < 64 || max_state_bytes.checked_mul(8).is_none() {
            bail!("state size limit must be at least 64 bytes and fit the memory budget");
        }
        let cache_path = prepare_path(&cache_path)?;
        let tokens_path = prepare_path(&tokens_path)?;
        if cache_path == tokens_path {
            bail!("cache and token files must have different paths");
        }
        seed_file(&cache_path, b"{\"systems\":[]}\n")?;
        seed_file(&tokens_path, b"{}\n")?;
        let snapshot = read_snapshot(&cache_path, &tokens_path, max_state_bytes)?;
        Ok(Self {
            inner: Arc::new(Inner {
                snapshot: ArcSwap::from_pointee(snapshot),
                writer: Arc::new(Mutex::new(())),
                cache_path,
                tokens_path,
                max_state_bytes,
            }),
        })
    }

    /// Return shared response bytes. Unknown status integers use one prepared
    /// empty response; arbitrary URLs can never grow the response cache.
    pub fn lookup(
        &self,
        system: Option<&str>,
        project: Option<&str>,
        target: Option<&str>,
        status: Option<i64>,
    ) -> std::result::Result<Bytes, LookupError> {
        if (system.is_none() && (project.is_some() || target.is_some() || status.is_some()))
            || (project.is_none() && (target.is_some() || status.is_some()))
            || (target.is_none() && status.is_some())
        {
            return Err(LookupError::InvalidPath);
        }
        let snapshot = self.inner.snapshot.load();
        let Some(system) = system else {
            return Ok(snapshot.prepared.body.clone());
        };
        let system = snapshot
            .prepared
            .systems
            .get(system)
            .ok_or(LookupError::SystemNotFound)?;
        let Some(project) = project else {
            return Ok(system.body.clone());
        };
        let project = system
            .projects
            .get(project)
            .ok_or(LookupError::ProjectNotFound)?;
        let Some(target) = target else {
            return Ok(project.body.clone());
        };
        let target = project
            .targets
            .get(target)
            .ok_or(LookupError::TargetNotFound)?;
        Ok(match status {
            Some(status) => target
                .statuses
                .get(&status)
                .unwrap_or(&target.empty)
                .clone(),
            None => target.body.clone(),
        })
    }

    /// Validate the raw Authorization value exactly as the original service did.
    /// Fixed-size digests make comparisons independent of token length; every
    /// token for the system is compared, avoiding an early-match timing branch.
    pub fn authorized(&self, system: &str, token: &str) -> bool {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return false;
        }
        let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let snapshot = self.inner.snapshot.load();
        let Some(tokens) = snapshot.token_digests.get(system) else {
            return false;
        };
        let mut matched = subtle::Choice::from(0);
        for expected in tokens {
            matched |= candidate.ct_eq(expected);
        }
        bool::from(matched)
    }

    pub fn snapshot(&self) -> Value {
        // The model contains only JSON-compatible types; conversion cannot fail.
        serde_json::to_value(&self.inner.snapshot.load().catalog)
            .expect("catalog model must remain serializable")
    }

    /// Full fingerprints are safe to list and can be passed to token_revoke.
    /// This never returns the token or a token prefix.
    pub fn token_fingerprints(&self, system: Option<&str>) -> Value {
        let snapshot = self.inner.snapshot.load();
        let mut result = serde_json::Map::new();
        for (name, tokens) in &snapshot.token_digests {
            if system.is_none_or(|requested| requested == name) {
                result.insert(
                    name.clone(),
                    Value::Array(
                        tokens
                            .iter()
                            .map(|digest| json!({"fingerprint": hex_digest(digest)}))
                            .collect(),
                    ),
                );
            }
        }
        Value::Object(result)
    }

    pub async fn upsert(
        &self,
        system: &str,
        project: &str,
        target: &str,
        payload: &Value,
    ) -> Result<()> {
        validate_segment(system)?;
        validate_segment(project)?;
        validate_segment(target)?;
        let payload = payload.as_object().context("JSON body must be an object")?;
        let heartbeat = payload
            .get("heartbeat")
            .and_then(Value::as_object)
            .context("Missing heartbeat data")?;
        let id = heartbeat
            .get("monitorID")
            .context("Invalid heartbeat data")?
            .clone();
        validate_id(&id)?;
        let status = heartbeat
            .get("status")
            .and_then(Value::as_i64)
            .context("Invalid heartbeat data")?;
        let monitor = payload.get("monitor").and_then(Value::as_object);
        let text = |field: &str| -> Result<String> {
            let Some(value) = monitor.and_then(|object| object.get(field)) else {
                return Ok("N/A".into());
            };
            let value = value.as_str().context("monitor fields must be strings")?;
            if value.len() > MAX_FIELD_BYTES {
                bail!("monitor field exceeds the length limit");
            }
            Ok(value.to_owned())
        };
        let network = Network {
            id,
            name: text("name")?,
            url: text("url")?,
            status,
        };
        let (system, project, target) = (system.to_owned(), project.to_owned(), target.to_owned());
        self.change(FileKind::Catalog, move |catalog, _| {
            catalog.upsert(system, project, target, network);
            Ok(())
        })
        .await
    }

    /// Remove one record or an explicitly selected subtree. Empty parents are
    /// pruned. A subsequent valid webhook can naturally recreate the record.
    pub async fn remove(
        &self,
        system: &str,
        project: Option<&str>,
        target: Option<&str>,
        id: Option<&Value>,
    ) -> Result<usize> {
        validate_segment(system)?;
        if let Some(value) = project {
            validate_segment(value)?;
        }
        if let Some(value) = target {
            validate_segment(value)?;
        }
        if (project.is_none() && (target.is_some() || id.is_some()))
            || (target.is_none() && id.is_some())
        {
            bail!("record removal requires system, project and target");
        }
        if let Some(value) = id {
            validate_id(value)?;
        }
        let system = system.to_owned();
        let project = project.map(str::to_owned);
        let target = target.map(str::to_owned);
        let id = id.cloned();
        self.change(FileKind::Catalog, move |catalog, _| {
            let mut removed = 0;
            for candidate_system in &mut catalog.systems {
                if candidate_system.system != system {
                    continue;
                }
                for candidate_project in &mut candidate_system.projects {
                    if project
                        .as_ref()
                        .is_some_and(|wanted| wanted != &candidate_project.project)
                    {
                        continue;
                    }
                    for candidate_target in &mut candidate_project.targets {
                        if target
                            .as_ref()
                            .is_some_and(|wanted| wanted != &candidate_target.target)
                        {
                            continue;
                        }
                        candidate_target.network.retain(|network| {
                            let matches = id.as_ref().is_none_or(|wanted| wanted == &network.id);
                            removed += usize::from(matches);
                            !matches
                        });
                    }
                }
            }
            catalog.prune();
            Ok(removed)
        })
        .await
    }

    pub async fn token_add(&self, system: &str, token: &str) -> Result<()> {
        validate_segment(system)?;
        validate_token(token)?;
        let (system, token) = (system.to_owned(), token.to_owned());
        self.change(FileKind::Tokens, move |_, tokens| {
            let entries = tokens.entry(system).or_default();
            if !entries.contains(&token) {
                if entries.len() >= MAX_TOKENS_PER_SYSTEM {
                    bail!("token count limit reached for this system");
                }
                entries.push(token);
            }
            Ok(())
        })
        .await
    }

    /// Revoke by full SHA-256 fingerprint, or by the exact token for compatibility.
    pub async fn token_revoke(&self, system: &str, selector: &str) -> Result<usize> {
        validate_segment(system)?;
        let (system, selector) = (system.to_owned(), selector.to_owned());
        self.change(FileKind::Tokens, move |_, tokens| {
            let Some(entries) = tokens.get_mut(&system) else {
                return Ok(0);
            };
            let before = entries.len();
            // Prefer an exact raw-token match over interpreting a token as a
            // fingerprint of another entry.
            if entries.iter().any(|token| token == &selector) {
                entries.retain(|token| token != &selector);
            } else {
                entries.retain(|token| {
                    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                    hex_digest(&digest) != selector
                });
            }
            let removed = before - entries.len();
            if entries.is_empty() {
                tokens.remove(&system);
            }
            Ok(removed)
        })
        .await
    }

    /// Reload replacement/modification events transactionally. Invalid input
    /// leaves the last complete snapshot active and returns an error to log.
    pub async fn reload(&self) -> Result<bool> {
        let guard = self.inner.writer.clone().lock_owned().await;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let current = inner.snapshot.load();
            if Some(FileStamp::read(&inner.cache_path)?) == current.cache_stamp
                && Some(FileStamp::read(&inner.tokens_path)?) == current.tokens_stamp
            {
                return Ok(false);
            }
            let snapshot =
                read_snapshot(&inner.cache_path, &inner.tokens_path, inner.max_state_bytes)?;
            let changed = snapshot.catalog != current.catalog || snapshot.tokens != current.tokens;
            inner.snapshot.store(Arc::new(snapshot));
            Ok(changed)
        })
        .await
        .context("state reload worker stopped")?
    }

    /// Wait for accepted mutation jobs. Request handlers must be drained before
    /// this barrier is used during shutdown.
    pub async fn flush(&self) -> Result<()> {
        let _guard = self.inner.writer.lock().await;
        Ok(())
    }

    async fn change<T, F>(&self, file: FileKind, change: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Catalog, &mut Tokens) -> Result<T> + Send + 'static,
    {
        let guard = self.inner.writer.clone().lock_owned().await;
        let inner = self.inner.clone();
        // Move the owned guard into the blocking job. Cancelling the request
        // cannot release the lock while its committed write is still running.
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let current = inner.snapshot.load_full();
            let mut catalog = current.catalog.clone();
            let mut tokens = current.tokens.clone();
            let result = change(&mut catalog, &mut tokens)?;
            if catalog == current.catalog && tokens == current.tokens {
                return Ok(result);
            }
            let mut candidate = Snapshot::new(catalog, tokens, inner.max_state_bytes)?;
            candidate.cache_stamp = current.cache_stamp;
            candidate.tokens_stamp = current.tokens_stamp;
            let (path, body) = match file {
                FileKind::Catalog => (
                    &inner.cache_path,
                    serde_json::to_vec_pretty(&candidate.catalog)?,
                ),
                FileKind::Tokens => (
                    &inner.tokens_path,
                    serde_json::to_vec_pretty(&candidate.tokens)?,
                ),
            };
            if body.len().saturating_add(1) > inner.max_state_bytes {
                bail!("persistent state exceeds configured size limit");
            }
            // Pre-rename errors leave disk and memory untouched. Once rename
            // succeeds, publish even if the final directory fsync fails: the
            // disk commit happened and memory must not contradict it.
            let (durability_error, stamp) = atomic_write(path, &body)?;
            match file {
                FileKind::Catalog => candidate.cache_stamp = stamp,
                FileKind::Tokens => candidate.tokens_stamp = stamp,
            }
            inner.snapshot.store(Arc::new(candidate));
            if let Some(error) = durability_error {
                return Err(error
                    .context("state committed but directory durability could not be confirmed"));
            }
            Ok(result)
        })
        .await
        .context("state mutation worker stopped")?
    }
}

impl Catalog {
    fn upsert(&mut self, system: String, project: String, target: String, network: Network) {
        let system_index = self.systems.iter().position(|item| item.system == system);
        let system = match system_index {
            Some(index) => &mut self.systems[index],
            None => {
                self.systems.push(System {
                    system,
                    projects: vec![],
                });
                self.systems.last_mut().expect("just inserted system")
            }
        };
        let project_index = system
            .projects
            .iter()
            .position(|item| item.project == project);
        let project = match project_index {
            Some(index) => &mut system.projects[index],
            None => {
                system.projects.push(Project {
                    project,
                    targets: vec![],
                });
                system.projects.last_mut().expect("just inserted project")
            }
        };
        let target_index = project
            .targets
            .iter()
            .position(|item| item.target == target);
        let target = match target_index {
            Some(index) => &mut project.targets[index],
            None => {
                project.targets.push(Target {
                    target,
                    network: vec![],
                });
                project.targets.last_mut().expect("just inserted target")
            }
        };
        match target.network.iter_mut().find(|item| item.id == network.id) {
            Some(existing) => *existing = network,
            None => target.network.push(network),
        }
    }

    fn prune(&mut self) {
        for system in &mut self.systems {
            for project in &mut system.projects {
                project.targets.retain(|target| !target.network.is_empty());
            }
            system
                .projects
                .retain(|project| !project.targets.is_empty());
        }
        self.systems.retain(|system| !system.projects.is_empty());
    }

    fn validate(&self) -> Result<()> {
        let mut systems = HashSet::new();
        for system in &self.systems {
            validate_segment(&system.system)?;
            if !systems.insert(&system.system) {
                bail!("duplicate system in cache");
            }
            let mut projects = HashSet::new();
            for project in &system.projects {
                validate_segment(&project.project)?;
                if !projects.insert(&project.project) {
                    bail!("duplicate project in cache");
                }
                let mut targets = HashSet::new();
                for target in &project.targets {
                    validate_segment(&target.target)?;
                    if !targets.insert(&target.target) {
                        bail!("duplicate target in cache");
                    }
                    let mut ids = HashSet::new();
                    for network in &target.network {
                        validate_id(&network.id)?;
                        if !ids.insert(serde_json::to_string(&network.id)?) {
                            bail!("duplicate network ID in cache target");
                        }
                        if network.name.len() > MAX_FIELD_BYTES
                            || network.url.len() > MAX_FIELD_BYTES
                        {
                            bail!("network metadata exceeds length limit");
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl Snapshot {
    fn new(catalog: Catalog, tokens: Tokens, limit: usize) -> Result<Self> {
        catalog.validate()?;
        let mut token_digests = BTreeMap::new();
        for (system, entries) in &tokens {
            validate_segment(system)?;
            if entries.len() > MAX_TOKENS_PER_SYSTEM {
                bail!("token count limit reached for a system");
            }
            let mut digests = Vec::with_capacity(entries.len());
            for token in entries {
                validate_token(token)?;
                digests.push(Sha256::digest(token.as_bytes()).into());
            }
            token_digests.insert(system.clone(), digests);
        }
        if serde_json::to_vec(&catalog)?.len() > limit || serde_json::to_vec(&tokens)?.len() > limit
        {
            bail!("state exceeds configured size limit");
        }
        let prepared = Prepared::build(&catalog, limit * 8)?;
        Ok(Self {
            catalog,
            tokens,
            token_digests,
            prepared,
            cache_stamp: None,
            tokens_stamp: None,
        })
    }
}

impl Prepared {
    fn build(catalog: &Catalog, max_response_bytes: usize) -> Result<Self> {
        let mut remaining = max_response_bytes;
        // Keep serialization budget accounting explicit at every hierarchy.
        let body = encode_response(catalog, &mut remaining)?;
        let mut systems = HashMap::new();
        for system in &catalog.systems {
            let system_body = encode_response(system, &mut remaining)?;
            let mut projects = HashMap::new();
            for project in &system.projects {
                let project_body = encode_response(project, &mut remaining)?;
                let mut targets = HashMap::new();
                for target in &project.targets {
                    let target_body = encode_response(target, &mut remaining)?;
                    let empty = encode_response(
                        &json!({"target":target.target,"network":[]}),
                        &mut remaining,
                    )?;
                    let mut by_status: BTreeMap<i64, Vec<&Network>> = BTreeMap::new();
                    for network in &target.network {
                        by_status.entry(network.status).or_default().push(network);
                    }
                    let mut statuses = HashMap::new();
                    for (status, network) in by_status {
                        statuses.insert(
                            status,
                            encode_response(
                                &json!({"target":target.target,"network":network}),
                                &mut remaining,
                            )?,
                        );
                    }
                    targets.insert(
                        target.target.clone(),
                        PreparedTarget {
                            body: target_body,
                            statuses,
                            empty,
                        },
                    );
                }
                projects.insert(
                    project.project.clone(),
                    PreparedProject {
                        body: project_body,
                        targets,
                    },
                );
            }
            systems.insert(
                system.system.clone(),
                PreparedSystem {
                    body: system_body,
                    projects,
                },
            );
        }
        Ok(Self { body, systems })
    }
}

fn encode_response<T: Serialize>(value: &T, remaining: &mut usize) -> Result<Bytes> {
    let mut body = serde_json::to_vec(value)?;
    body.push(b'\n');
    *remaining = remaining
        .checked_sub(body.len())
        .context("prepared responses exceed memory budget")?;
    Ok(Bytes::from(body))
}

fn validate_segment(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_SEGMENT_BYTES
        || value.contains('/')
        || value.chars().any(char::is_control)
    {
        bail!("path segment must contain 1–256 bytes, without slashes or control characters");
    }
    Ok(())
}

fn validate_id(value: &Value) -> Result<()> {
    if value.as_i64().is_some() || value.as_u64().is_some() {
        return Ok(());
    }
    if value.as_str().is_some_and(|id| {
        !id.is_empty() && id.len() <= MAX_SEGMENT_BYTES && !id.chars().any(char::is_control)
    }) {
        return Ok(());
    }
    bail!("monitorID must be an integer or a nonempty string of at most 256 bytes")
}

fn validate_token(token: &str) -> Result<()> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.chars().any(char::is_control) {
        bail!("token must contain 1–4096 bytes without control characters");
    }
    Ok(())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 15) as usize] as char);
    }
    output
}

fn prepare_path(path: &Path) -> Result<PathBuf> {
    let filename = path
        .file_name()
        .context("state path must identify a file")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("cannot create state directory {}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .context("cannot resolve state directory")?;
    let path = parent.join(filename);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if !metadata.file_type().is_file() {
            bail!("state path must be a regular file, not a symlink or directory");
        }
    }
    Ok(path)
}

fn seed_file(path: &Path, body: &[u8]) -> Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(body)?;
            file.sync_all()?;
            File::open(path.parent().context("state file has no parent")?)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).context("cannot initialize state file"),
    }
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).context("cannot inspect state file")?;
    if !metadata.file_type().is_file() || metadata.len() > limit as u64 {
        bail!("state file must be a regular file within the configured size limit");
    }
    let mut body = Vec::new();
    File::open(path)
        .context("cannot open state file")?
        .take(limit as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > limit {
        bail!("state file exceeds configured size limit");
    }
    Ok(body)
}

fn read_catalog(path: &Path, limit: usize) -> Result<Catalog> {
    let body = read_limited(path, limit)?;
    serde_json::from_slice(&body).map_err(|error| {
        anyhow!(
            "cache has invalid JSON/schema at line {}, column {}",
            error.line(),
            error.column()
        )
    })
}

fn read_tokens(path: &Path, limit: usize) -> Result<Tokens> {
    let body = read_limited(path, limit)?;
    // serde errors can include a rejected string value. Do not expose token
    // material in error messages that the application subsequently logs.
    serde_json::from_slice(&body).map_err(|error| {
        anyhow!(
            "tokens have invalid JSON/schema at line {}, column {}",
            error.line(),
            error.column()
        )
    })
}

fn read_snapshot(cache_path: &Path, tokens_path: &Path, limit: usize) -> Result<Snapshot> {
    let cache_stamp = FileStamp::read(cache_path)?;
    let tokens_stamp = FileStamp::read(tokens_path)?;
    let catalog = read_catalog(cache_path, limit)?;
    let tokens = read_tokens(tokens_path, limit)?;
    if cache_stamp != FileStamp::read(cache_path)? || tokens_stamp != FileStamp::read(tokens_path)?
    {
        bail!("state files changed during reload; retaining previous snapshot and retrying later");
    }
    let mut snapshot = Snapshot::new(catalog, tokens, limit)?;
    snapshot.cache_stamp = Some(cache_stamp);
    snapshot.tokens_stamp = Some(tokens_stamp);
    Ok(snapshot)
}

/// Returns a post-commit durability error separately from pre-commit failures.
fn atomic_write(path: &Path, body: &[u8]) -> Result<(Option<anyhow::Error>, Option<FileStamp>)> {
    let parent = path.parent().context("state file has no parent")?;
    let filename = path
        .file_name()
        .context("state path has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{filename}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> Result<(Option<anyhow::Error>, Option<FileStamp>)> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .context("cannot create temporary state file")?;
        file.write_all(body).context("cannot write state")?;
        file.write_all(b"\n")?;
        file.sync_all().context("cannot synchronize state file")?;
        // Check directory sync capability before the commit point as well.
        let directory = File::open(parent).context("cannot open state directory")?;
        directory
            .sync_all()
            .context("cannot synchronize state directory")?;
        fs::rename(&temporary, path).context("cannot atomically replace state")?;
        let stamp = file
            .metadata()
            .ok()
            .map(|metadata| FileStamp::from_metadata(&metadata));
        Ok((directory.sync_all().err().map(anyhow::Error::from), stamp))
    };
    let result = write();
    // Also remove a partially written temporary file on pre-commit failure.
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Engine) {
        let directory = TempDir::new().unwrap();
        let engine = Engine::open(
            directory.path().join("cache.json"),
            directory.path().join("tokens.json"),
            1024 * 1024,
        )
        .unwrap();
        (directory, engine)
    }

    fn heartbeat(id: Value, status: i64) -> Value {
        json!({
            "heartbeat":{"monitorID":id,"status":status,"msg":"reachable"},
            "monitor":{"name":"mirror","url":"https://example.invalid/code"}
        })
    }

    fn decode(bytes: Bytes) -> Value {
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn fresh_installation_has_empty_legacy_compatible_files() {
        let (directory, engine) = fixture();
        assert_eq!(engine.snapshot(), json!({"systems":[]}));
        assert_eq!(
            decode(engine.lookup(None, None, None, None).unwrap()),
            engine.snapshot()
        );
        assert!(!engine.authorized("community", ""));
        assert_eq!(
            fs::metadata(directory.path().join("tokens.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            engine.lookup(Some("missing"), None, None, None),
            Err(LookupError::SystemNotFound)
        );
    }

    #[tokio::test]
    async fn webhook_grows_every_hierarchy_and_persists_before_acknowledgement() {
        let (directory, engine) = fixture();
        engine
            .upsert("community", "jcb", "powers", &heartbeat(json!(12), 1))
            .await
            .unwrap();
        let expected =
            json!({"id":12,"name":"mirror","url":"https://example.invalid/code","status":1});
        let target = decode(
            engine
                .lookup(Some("community"), Some("jcb"), Some("powers"), None)
                .unwrap(),
        );
        assert_eq!(target, json!({"target":"powers","network":[expected]}));
        let project = decode(
            engine
                .lookup(Some("community"), Some("jcb"), None, None)
                .unwrap(),
        );
        assert_eq!(project["targets"][0], target);
        let system = decode(engine.lookup(Some("community"), None, None, None).unwrap());
        assert_eq!(system["projects"][0], project);
        let all = decode(engine.lookup(None, None, None, None).unwrap());
        assert_eq!(all["systems"][0], system);
        let disk: Value =
            serde_json::from_slice(&fs::read(directory.path().join("cache.json")).unwrap())
                .unwrap();
        assert_eq!(disk, all);

        let reopened = Engine::open(
            directory.path().join("cache.json"),
            directory.path().join("tokens.json"),
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(reopened.snapshot(), all);
    }

    #[tokio::test]
    async fn cached_reads_share_bytes_and_unknown_statuses_do_not_expand_cache() {
        let (_directory, engine) = fixture();
        engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 1))
            .await
            .unwrap();
        let a = engine
            .lookup(Some("s"), Some("p"), Some("t"), None)
            .unwrap();
        let b = engine
            .lookup(Some("s"), Some("p"), Some("t"), None)
            .unwrap();
        assert_eq!(a.as_ptr(), b.as_ptr());
        let empty = engine
            .lookup(Some("s"), Some("p"), Some("t"), Some(5))
            .unwrap();
        assert_eq!(decode(empty.clone()), json!({"target":"t","network":[]}));
        for status in -50..0 {
            assert_eq!(
                empty.as_ptr(),
                engine
                    .lookup(Some("s"), Some("p"), Some("t"), Some(status))
                    .unwrap()
                    .as_ptr()
            );
        }
    }

    #[tokio::test]
    async fn upsert_replaces_existing_id_and_preserves_network_insertion_order() {
        let (_directory, engine) = fixture();
        for id in [json!(1), json!(2), json!("1")] {
            engine
                .upsert("s", "p", "t", &heartbeat(id, 1))
                .await
                .unwrap();
        }
        engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 0))
            .await
            .unwrap();
        let target = decode(
            engine
                .lookup(Some("s"), Some("p"), Some("t"), None)
                .unwrap(),
        );
        assert_eq!(target["network"].as_array().unwrap().len(), 3);
        assert_eq!(target["network"][0]["id"], 1);
        assert_eq!(target["network"][0]["status"], 0);
        assert_eq!(target["network"][1]["id"], 2);
        assert_eq!(target["network"][2]["id"], "1");
        let online = decode(
            engine
                .lookup(Some("s"), Some("p"), Some("t"), Some(1))
                .unwrap(),
        );
        assert_eq!(online["network"].as_array().unwrap().len(), 2);
        let offline = decode(
            engine
                .lookup(Some("s"), Some("p"), Some("t"), Some(0))
                .unwrap(),
        );
        assert_eq!(offline["network"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn legacy_arbitrary_integer_statuses_remain_filterable() {
        let (_directory, engine) = fixture();
        engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 2))
            .await
            .unwrap();
        assert_eq!(
            decode(
                engine
                    .lookup(Some("s"), Some("p"), Some("t"), Some(2))
                    .unwrap()
            )["network"][0]["status"],
            2
        );
        assert!(decode(
            engine
                .lookup(Some("s"), Some("p"), Some("t"), Some(0))
                .unwrap()
        )["network"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn removal_prunes_empty_parents_and_can_be_recreated_by_webhook() {
        let (_directory, engine) = fixture();
        engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 1))
            .await
            .unwrap();
        engine
            .upsert("s", "p", "t", &heartbeat(json!(2), 0))
            .await
            .unwrap();
        assert_eq!(
            engine
                .remove("s", Some("p"), Some("t"), Some(&json!(1)))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            engine
                .remove("s", Some("p"), Some("t"), Some(&json!(999)))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            engine
                .remove("s", Some("p"), Some("t"), None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            engine.lookup(Some("s"), None, None, None),
            Err(LookupError::SystemNotFound)
        );
        engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 1))
            .await
            .unwrap();
        assert!(engine.lookup(Some("s"), Some("p"), Some("t"), None).is_ok());
        assert_eq!(engine.remove("s", None, None, None).await.unwrap(), 1);
        assert_eq!(engine.snapshot(), json!({"systems":[]}));
    }

    #[tokio::test]
    async fn subtree_removal_does_not_touch_other_projects_or_systems() {
        let (_directory, engine) = fixture();
        for (s, p, t) in [
            ("s", "p", "a"),
            ("s", "p", "b"),
            ("s", "q", "a"),
            ("other", "p", "a"),
        ] {
            engine
                .upsert(s, p, t, &heartbeat(json!(1), 1))
                .await
                .unwrap();
        }
        assert_eq!(engine.remove("s", Some("p"), None, None).await.unwrap(), 2);
        assert_eq!(
            engine.lookup(Some("s"), Some("p"), None, None),
            Err(LookupError::ProjectNotFound)
        );
        assert!(engine.lookup(Some("s"), Some("q"), None, None).is_ok());
        assert!(engine.lookup(Some("other"), Some("p"), None, None).is_ok());
    }

    #[tokio::test]
    async fn token_matching_is_exact_scoped_and_listing_never_exposes_secrets() {
        let (directory, engine) = fixture();
        engine
            .token_add("s", "existing-secret-token")
            .await
            .unwrap();
        engine
            .token_add("s", "existing-secret-token")
            .await
            .unwrap();
        engine.token_add("s", "Bearer legacy-value").await.unwrap();
        assert!(engine.authorized("s", "existing-secret-token"));
        assert!(engine.authorized("s", "Bearer legacy-value"));
        assert!(!engine.authorized("s", "secret"));
        assert!(!engine.authorized("other", "existing-secret-token"));
        assert!(!engine.authorized("s", "legacy-value"));
        let fingerprints = engine.token_fingerprints(Some("s"));
        assert_eq!(fingerprints["s"].as_array().unwrap().len(), 2);
        assert!(!fingerprints.to_string().contains("existing-secret-token"));
        let fingerprint = fingerprints["s"][0]["fingerprint"].as_str().unwrap();
        assert_eq!(engine.token_revoke("s", fingerprint).await.unwrap(), 1);
        assert!(!engine.authorized("s", "existing-secret-token"));
        assert!(engine.authorized("s", "Bearer legacy-value"));
        let reopened = Engine::open(
            directory.path().join("cache.json"),
            directory.path().join("tokens.json"),
            1024 * 1024,
        )
        .unwrap();
        assert!(!reopened.authorized("s", "existing-secret-token"));
        assert!(reopened.authorized("s", "Bearer legacy-value"));
    }

    #[tokio::test]
    async fn malformed_webhooks_cannot_mutate_state() {
        let (_directory, engine) = fixture();
        for payload in [
            json!([1]),
            json!({}),
            json!({"heartbeat":null}),
            json!({"heartbeat":{"monitorID":[],"status":1}}),
            json!({"heartbeat":{"monitorID":1,"status":"1"}}),
            json!({"heartbeat":{"monitorID":null,"status":1}}),
            json!({"heartbeat":{"monitorID":1,"status":1},"monitor":{"name":[]}}),
        ] {
            assert!(engine.upsert("s", "p", "t", &payload).await.is_err());
            assert_eq!(engine.snapshot(), json!({"systems":[]}));
        }
        assert!(engine
            .upsert("s/p", "p", "t", &heartbeat(json!(1), 1))
            .await
            .is_err());
        assert!(engine.remove("s", None, Some("t"), None).await.is_err());
    }

    #[tokio::test]
    async fn missing_monitor_fields_keep_legacy_defaults() {
        let (_directory, engine) = fixture();
        engine
            .upsert(
                "s",
                "p",
                "t",
                &json!({"heartbeat":{"monitorID":1,"status":1}}),
            )
            .await
            .unwrap();
        let target = decode(
            engine
                .lookup(Some("s"), Some("p"), Some("t"), None)
                .unwrap(),
        );
        assert_eq!(target["network"][0]["name"], "N/A");
        assert_eq!(target["network"][0]["url"], "N/A");
    }

    #[tokio::test]
    async fn failed_persistence_keeps_previous_public_snapshot() {
        let (directory, engine) = fixture();
        engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 1))
            .await
            .unwrap();
        let previous = engine.snapshot();
        let cache = directory.path().join("cache.json");
        let backup = directory.path().join("original.json");
        fs::rename(&cache, &backup).unwrap();
        fs::create_dir(&cache).unwrap();
        assert!(engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 0))
            .await
            .is_err());
        assert_eq!(engine.snapshot(), previous);
        assert_eq!(
            fs::read_dir(directory.path())
                .unwrap()
                .filter_map(|item| item.ok())
                .filter(|item| item.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
        fs::remove_dir(&cache).unwrap();
        fs::rename(&backup, &cache).unwrap();
        assert_eq!(
            read_catalog(&cache, 1024 * 1024).unwrap(),
            engine.inner.snapshot.load().catalog
        );
    }

    #[tokio::test]
    async fn malformed_external_reload_keeps_last_valid_state_and_tokens() {
        let (directory, engine) = fixture();
        engine.token_add("s", "safe-existing-token").await.unwrap();
        fs::write(
            directory.path().join("tokens.json"),
            b"{\"s\":\"safe-existing-token\"}",
        )
        .unwrap();
        let error = engine.reload().await.unwrap_err().to_string();
        assert!(!error.contains("safe-existing-token"));
        assert!(engine.authorized("s", "safe-existing-token"));
        fs::write(
            directory.path().join("tokens.json"),
            b"{\"s\":[\"replacement\"]}",
        )
        .unwrap();
        fs::write(directory.path().join("cache.json"), b"[]").unwrap();
        assert!(engine.reload().await.is_err());
        assert!(engine.authorized("s", "safe-existing-token"));
        fs::write(directory.path().join("cache.json"), b"{\"systems\":[]}").unwrap();
        assert!(engine.reload().await.unwrap());
        assert!(engine.authorized("s", "replacement"));
        assert!(!engine.authorized("s", "safe-existing-token"));
        assert!(!engine.reload().await.unwrap());
    }

    #[tokio::test]
    async fn atomic_replacement_reload_applies_token_revocations() {
        let (directory, engine) = fixture();
        engine.token_add("s", "old").await.unwrap();
        let replacement = directory.path().join("replacement.json");
        fs::write(&replacement, b"{\"s\":[\"new\"]}").unwrap();
        fs::rename(replacement, directory.path().join("tokens.json")).unwrap();
        assert!(engine.reload().await.unwrap());
        assert!(!engine.authorized("s", "old"));
        assert!(engine.authorized("s", "new"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_writes_do_not_lose_dynamic_additions() {
        let (_directory, engine) = fixture();
        let mut tasks = Vec::new();
        for id in 0..24 {
            let engine = engine.clone();
            tasks.push(tokio::spawn(async move {
                engine
                    .upsert("s", "p", "t", &heartbeat(json!(id), id % 2))
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        engine.flush().await.unwrap();
        let target = decode(
            engine
                .lookup(Some("s"), Some("p"), Some("t"), None)
                .unwrap(),
        );
        assert_eq!(target["network"].as_array().unwrap().len(), 24);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_request_does_not_interrupt_its_started_write() {
        let (_directory, engine) = fixture();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = engine.clone();
        let request = tokio::spawn(async move {
            writer
                .change(FileKind::Catalog, move |catalog, _| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    catalog.upsert(
                        "s".into(),
                        "p".into(),
                        "t".into(),
                        Network {
                            id: json!(1),
                            name: "n".into(),
                            url: "u".into(),
                            status: 1,
                        },
                    );
                    Ok(())
                })
                .await
        });
        entered_rx.await.unwrap();
        request.abort();
        release_tx.send(()).unwrap();
        engine.flush().await.unwrap();
        assert!(engine.lookup(Some("s"), Some("p"), Some("t"), None).is_ok());
    }

    #[tokio::test]
    async fn state_size_limit_rejects_growth_without_touching_disk() {
        let directory = TempDir::new().unwrap();
        let engine = Engine::open(
            directory.path().join("cache.json"),
            directory.path().join("tokens.json"),
            64,
        )
        .unwrap();
        assert!(engine
            .upsert("s", "p", "t", &heartbeat(json!(1), 1))
            .await
            .is_err());
        assert_eq!(engine.snapshot(), json!({"systems":[]}));
        assert_eq!(
            read_catalog(&directory.path().join("cache.json"), 64).unwrap(),
            Catalog::default()
        );
    }

    #[test]
    fn symlinked_state_and_shared_cache_token_paths_are_rejected() {
        let directory = TempDir::new().unwrap();
        fs::write(directory.path().join("real.json"), b"{\"systems\":[]}").unwrap();
        symlink(
            directory.path().join("real.json"),
            directory.path().join("cache.json"),
        )
        .unwrap();
        assert!(Engine::open(
            directory.path().join("cache.json"),
            directory.path().join("tokens.json"),
            1024
        )
        .is_err());
        assert!(Engine::open(
            directory.path().join("same.json"),
            directory.path().join("same.json"),
            1024
        )
        .is_err());
    }

    #[test]
    fn malformed_token_schema_never_becomes_substring_authorization() {
        let directory = TempDir::new().unwrap();
        fs::write(
            directory.path().join("tokens.json"),
            b"{\"s\":\"long-secret\"}",
        )
        .unwrap();
        assert!(Engine::open(
            directory.path().join("cache.json"),
            directory.path().join("tokens.json"),
            1024
        )
        .is_err());
    }
}
