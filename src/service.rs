//! Unix-socket HTTP service and a separate owner-only local control protocol.
use crate::{config::Config, documentation, engine::Engine};
use anyhow::{bail, Context, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
    Router,
};
use bytes::Bytes;
use fs2::FileExt;
use percent_encoding::percent_decode_str;
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    net::{UnixListener, UnixStream},
    sync::{watch, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub config: Config,
    pub openapi: Bytes,
}
impl AppState {
    pub fn new(engine: Engine, config: Config) -> Result<Self> {
        let mut spec = documentation::openapi();
        if let Some(url) = &config.public_base_url {
            spec["servers"] = json!([{"url":url}]);
        }
        Ok(Self {
            engine,
            config,
            openapi: Bytes::from(serde_json::to_vec(&spec)?),
        })
    }
}
pub fn router(state: AppState) -> Router {
    Router::new().fallback(public).with_state(state)
}
fn response(status: StatusCode, body: impl Into<Body>, content_type: &str) -> Response {
    let mut res = Response::new(body.into());
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("static MIME type"),
    );
    res.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}
fn json_response(status: StatusCode, value: Value) -> Response {
    response(status, value.to_string(), "application/json")
}
fn error(status: StatusCode, msg: &str) -> Response {
    json_response(status, json!({"error":msg}))
}
fn landing() -> Response {
    response(
        StatusCode::OK,
        documentation::html(),
        "text/html; charset=utf-8",
    )
}
fn cors(res: &mut Response, headers: &HeaderMap) {
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        headers
            .get(header::ORIGIN)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("*")),
    );
    res.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));
}
async fn public(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let mut res = public_inner(&state, req).await;
    cors(&mut res, &headers);
    if method == Method::HEAD {
        use axum::body::HttpBody;
        if let Some(size) = res.body().size_hint().exact() {
            if let Ok(value) = HeaderValue::from_str(&size.to_string()) {
                res.headers_mut().insert(header::CONTENT_LENGTH, value);
            }
        }
        *res.body_mut() = Body::empty();
    }
    res
}
async fn public_inner(state: &AppState, req: Request) -> Response {
    let path = match percent_decode_str(req.uri().path()).decode_utf8() {
        Ok(v) => v.into_owned(),
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid URL encoding"),
    };
    if req.method() == Method::OPTIONS {
        let mut res = response(StatusCode::OK, Body::empty(), "text/html; charset=utf-8");
        res.headers_mut().insert(
            header::ALLOW,
            HeaderValue::from_static("GET, HEAD, OPTIONS, POST"),
        );
        res.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT"),
        );
        if let Some(v) = req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
            res.headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v.clone());
        }
        return res;
    }
    if path == "/healthz" && req.method() == Method::GET {
        return json_response(
            StatusCode::OK,
            json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}),
        );
    }
    if let Some(rest) = path.strip_prefix("/v1/webhook/") {
        let parts: Vec<_> = rest.split('/').collect();
        if parts.len() != 3 || parts.iter().any(|s| s.is_empty()) {
            return landing();
        }
        if req.method() != Method::POST {
            return error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
        }
        return webhook(state, parts[0], parts[1], parts[2], req).await;
    }
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    if path == "/openapi.json" || path == "/OpenAPI.json" {
        return response(StatusCode::OK, state.openapi.clone(), "application/json");
    }
    if path == "/v1/network" {
        return redirect(state, &path, req.uri().query());
    }
    let Some(rest) = path.strip_prefix("/v1/network/") else {
        return landing();
    };
    let mut parts: Vec<_> = rest.split('/').collect();
    if rest.is_empty() {
        parts.clear();
    } else if (parts.len() == 1 || parts.len() == 2) && !rest.ends_with('/') {
        return redirect(state, &path, req.uri().query());
    } else if rest.ends_with('/') && parts.len() <= 3 {
        parts.pop();
    } else if rest.ends_with('/') {
        return landing();
    }
    if parts.len() > 4 || parts.iter().any(|s| s.is_empty()) {
        return landing();
    }
    let system = parts.first().copied();
    let project = parts.get(1).copied();
    let target = parts.get(2).copied();
    let status = parts.get(3).copied();
    if state.config.log_requests {
        tracing::info!(
            system = system.unwrap_or("None"),
            project = project.unwrap_or("None"),
            target = target.unwrap_or("None"),
            status = status.unwrap_or("None"),
            "Network request received"
        );
    }
    // Resolve the hierarchy before validating the status, as the original API does.
    let base = match state.engine.lookup(system, project, target, None) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%e,"Network lookup failed");
            return error(StatusCode::FORBIDDEN, &e.to_string());
        }
    };
    let result = if let Some(raw) = status {
        let trimmed = raw.trim();
        let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return error(StatusCode::BAD_REQUEST, "Invalid status provided");
        }
        if let Ok(status) = trimmed.parse::<i64>() {
            match state.engine.lookup(system, project, target, Some(status)) {
                Ok(v) => v,
                Err(e) => return error(StatusCode::FORBIDDEN, &e.to_string()),
            }
        } else {
            // Python accepts arbitrarily large integers; these match no stored status.
            match state.engine.empty_status_response(
                system.expect("status route has a system"),
                project.expect("status route has a project"),
                target.expect("status route has a target"),
            ) {
                Ok(v) => v,
                Err(e) => return error(StatusCode::FORBIDDEN, &e.to_string()),
            }
        }
    } else {
        base
    };
    let mut res = response(StatusCode::OK, result, "application/json");
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}
fn redirect(state: &AppState, path: &str, query: Option<&str>) -> Response {
    let mut destination = format!(
        "{}{path}/",
        state
            .config
            .public_base_url
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/')
    );
    if let Some(q) = query {
        destination.push('?');
        destination.push_str(q);
    }
    let mut res = response(
        StatusCode::PERMANENT_REDIRECT,
        Body::empty(),
        "text/html; charset=utf-8",
    );
    if let Ok(value) = HeaderValue::from_str(&destination) {
        res.headers_mut().insert(header::LOCATION, value);
    }
    res
}
async fn webhook(
    state: &AppState,
    system: &str,
    project: &str,
    target: &str,
    req: Request,
) -> Response {
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .map(|t| state.engine.authorized(system, t))
        .unwrap_or(false);
    if !authorized {
        return error(StatusCode::FORBIDDEN, "Unauthorized");
    }
    let bytes = match tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(req.into_body(), state.config.max_body_bytes),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "Request body timed out"),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "No JSON data provided"),
    };
    if value.as_object().is_none_or(|v| v.is_empty()) {
        return error(StatusCode::BAD_REQUEST, "No JSON data provided");
    }
    let Some(heartbeat) = value.get("heartbeat").and_then(Value::as_object) else {
        return error(StatusCode::BAD_REQUEST, "Missing heartbeat data");
    };
    let Some(id) = heartbeat.get("monitorID") else {
        return error(StatusCode::BAD_REQUEST, "Invalid heartbeat data");
    };
    let Some(status) = heartbeat.get("status").and_then(Value::as_i64) else {
        return error(StatusCode::BAD_REQUEST, "Invalid heartbeat data");
    };
    let valid_id = id.is_i64()
        || id.is_u64()
        || id
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control));
    if !valid_id {
        return error(StatusCode::BAD_REQUEST, "Invalid heartbeat data");
    }
    if [system, project, target].iter().any(|s| {
        s.is_empty() || s.len() > 256 || s.contains('/') || s.chars().any(char::is_control)
    }) {
        return error(StatusCode::BAD_REQUEST, "Invalid network path");
    }
    let monitor = value.get("monitor").and_then(Value::as_object);
    for field in ["name", "url"] {
        if let Some(v) = monitor.and_then(|m| m.get(field)) {
            if v.as_str().is_none_or(|s| s.len() > 16 * 1024) {
                return error(StatusCode::BAD_REQUEST, "Invalid monitor data");
            }
        }
    }
    let msg = value
        .get("msg")
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .or_else(|| heartbeat.get("msg"));
    let msg = bounded_log_message(msg);
    tracing::info!(system,project,target,monitor_id=%id,status,msg=%msg,monitor_name=%monitor.and_then(|m|m.get("name")).unwrap_or(&serde_json::Value::Null),url=%monitor.and_then(|m|m.get("url")).unwrap_or(&serde_json::Value::Null),"Webhook request received");
    match state.engine.upsert(system, project, target, &value).await {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({"message":format!("Status updated to {}",if status==1{"up"}else{"down"})}),
        ),
        Err(e) => {
            tracing::error!(error=%e,"Status update failed");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to persist status update",
            )
        }
    }
}

/// Keep normal webhook details while ensuring a message cannot occupy an entire
/// request-sized record in the asynchronous log queue. Only message strings are
/// logged; unexpected JSON objects are not recursively serialized into logs.
fn bounded_log_message(value: Option<&Value>) -> String {
    const LIMIT: usize = 16 * 1024;
    let text = match value {
        None => "N/A",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => "[non-string message]",
    };
    if text.len() <= LIMIT {
        return text.to_owned();
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", &text[..end])
}

/// A permit lives as long as its connection, so slow clients cannot cause an
/// unbounded population of HTTP tasks. Nginx additionally controls public rates.
struct LimitedStream {
    stream: UnixStream,
    _permit: OwnedSemaphorePermit,
}
impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
struct LimitedListener {
    listener: UnixListener,
    permits: Arc<Semaphore>,
}
impl axum::serve::Listener for LimitedListener {
    type Io = LimitedStream;
    type Addr = tokio::net::unix::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore remains open");
            match self.listener.accept().await {
                Ok((stream, addr)) => {
                    return (
                        LimitedStream {
                            stream,
                            _permit: permit,
                        },
                        addr,
                    )
                }
                Err(e) => {
                    tracing::error!(error=%e,"Socket accept failed");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
async fn bind_socket(path: &Path, mode: u32) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                bail!("Refusing to replace non-socket {}", path.display());
            }
            // A live listener with a full backlog must not block startup's
            // runtime thread. Only a definite refusal permits stale cleanup;
            // a timeout or any other error preserves the existing socket.
            let probe = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path))
                .await
                .with_context(|| format!("Timed out checking socket {}", path.display()))?;
            match probe {
                Ok(_) => bail!("Socket {} is already in use", path.display()),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {}
                Err(e) => {
                    return Err(e).context("Cannot determine whether existing socket is active")
                }
            }
            fs::remove_file(path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(listener)
}
fn state_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(PathBuf::from(name))?;
    FileExt::try_lock_exclusive(&file).context("Another service already owns these state files")?;
    Ok(file)
}
async fn stop(mut rx: watch::Receiver<bool>) {
    if !*rx.borrow() {
        let _ = rx.changed().await;
    }
}
async fn signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {_ = term.recv()=>{},r=tokio::signal::ctrl_c()=>r?}
    Ok(())
}
pub async fn serve(config: Config) -> Result<()> {
    let _locks = [state_lock(&config.cache)?, state_lock(&config.tokens)?];
    let engine = Engine::open(
        config.cache.clone(),
        config.tokens.clone(),
        config.max_state_bytes,
    )?;
    let state = AppState::new(engine.clone(), config.clone())?;
    let api = bind_socket(&config.api_socket, 0o660).await?;
    let admin = bind_socket(&config.admin_socket, 0o600).await?;
    let (tx, rx) = watch::channel(false);
    let mut api_task = tokio::spawn(
        axum::serve(
            LimitedListener {
                listener: api,
                permits: Arc::new(Semaphore::new(config.max_connections)),
            },
            router(state),
        )
        .with_graceful_shutdown(stop(rx.clone()))
        .into_future(),
    );
    let mut admin_task = tokio::spawn(admin_loop(admin, engine.clone(), rx.clone()));
    let watch_engine = engine.clone();
    let watch_config = config.clone();
    let watch_rx = rx.clone();
    let watcher = tokio::spawn(async move {
        loop {
            tokio::select! {_ = stop(watch_rx.clone())=>break,_=tokio::time::sleep(Duration::from_secs(watch_config.poll_seconds))=>{match watch_engine.reload().await{Ok(true)=>tracing::info!("External state reloaded"),Ok(false)=>{},Err(e)=>tracing::error!(error=%e,"External state rejected; retaining valid state")}}}
        }
    });
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        cpu = config.cpu_count,
        memory_mb = config.memory_mb,
        "API service started"
    );
    let mut failure = None;
    let mut api_done = false;
    let mut admin_done = false;
    tokio::select! {r=signal()=>{if let Err(e)=r{failure=Some(e);}},r=&mut api_task=>{api_done=true;failure=Some(anyhow::anyhow!("HTTP server exited: {r:?}"));},r=&mut admin_task=>{admin_done=true;failure=Some(anyhow::anyhow!("Administration server exited: {r:?}"));}}
    let _ = tx.send(true);
    if !api_done {
        api_task.await??;
    }
    if !admin_done {
        admin_task.await??;
    }
    watcher.await?;
    engine.flush().await?;
    let _ = fs::remove_file(&config.api_socket);
    let _ = fs::remove_file(&config.admin_socket);
    tracing::info!("API service stopped; state persisted");
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(())
}
use std::future::IntoFuture;
async fn admin_loop(
    listener: UnixListener,
    engine: Engine,
    rx: watch::Receiver<bool>,
) -> Result<()> {
    let permits = Arc::new(Semaphore::new(16));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {_ = stop(rx.clone())=>break,Some(r)=tasks.join_next(),if !tasks.is_empty()=>{if let Err(e)=r{tracing::error!(error=%e,"Administration task failed");}},r=listener.accept()=>{let (socket,_)=r?;if let Ok(permit)=permits.clone().try_acquire_owned(){let engine=engine.clone();tasks.spawn(async move{let _permit=permit;let _=tokio::time::timeout(Duration::from_secs(30),admin_connection(socket,engine)).await;});}}}
    }
    while let Some(r) = tasks.join_next().await {
        r?;
    }
    Ok(())
}
async fn admin_connection(socket: UnixStream, engine: Engine) -> Result<()> {
    let mut reader = BufReader::new(socket);
    let mut bytes = Vec::new();
    let count = (&mut reader)
        .take(65_537)
        .read_until(b'\n', &mut bytes)
        .await?;
    if count > 65_536 || bytes.last() != Some(&b'\n') {
        bail!("Invalid administration request size");
    }
    let result = match serde_json::from_slice::<Value>(&bytes) {
        Ok(command) => admin_command(&engine, command).await,
        Err(e) => Err(e.into()),
    };
    let payload = match result {
        Ok(result) => json!({"ok":true,"result":result}),
        Err(e) => {
            tracing::warn!(error=%e,"Administration command failed");
            json!({"ok":false,"error":e.to_string()})
        }
    };
    let mut output = serde_json::to_vec(&payload)?;
    output.push(b'\n');
    reader.get_mut().write_all(&output).await?;
    Ok(())
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing {key}"))
}
async fn admin_command(engine: &Engine, cmd: Value) -> Result<Value> {
    let op = required(&cmd, "op")?;
    for key in ["project", "target"] {
        if cmd.get(key).is_some_and(|v| !v.is_null() && !v.is_string()) {
            bail!("Invalid {key}: expected a string");
        }
    }
    let result = match op {
        "network.list" => engine.snapshot(),
        "network.remove" => {
            let n = engine
                .remove(
                    required(&cmd, "system")?,
                    cmd.get("project").and_then(Value::as_str),
                    cmd.get("target").and_then(Value::as_str),
                    cmd.get("id").filter(|v| !v.is_null()),
                )
                .await?;
            json!({"removed":n})
        }
        "token.create" => {
            let mut bytes = [0u8; 32];
            OsRng.fill_bytes(&mut bytes);
            let token = hex::encode(bytes);
            engine.token_add(required(&cmd, "system")?, &token).await?;
            json!({"token":token})
        }
        "token.list" => engine.token_fingerprints(cmd.get("system").and_then(Value::as_str)),
        "token.revoke" => {
            let selector = required(&cmd, "fingerprint")?;
            if selector.len() != 64 || !selector.bytes().all(|c| c.is_ascii_hexdigit()) {
                bail!("Provide a full SHA-256 fingerprint");
            }
            let n = engine
                .token_revoke(required(&cmd, "system")?, selector)
                .await?;
            json!({"revoked":n})
        }
        "reload" => json!({"changed":engine.reload().await?}),
        "check" => json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}),
        _ => bail!("Unknown administration operation"),
    };
    if op != "network.list" && op != "token.list" && op != "check" {
        tracing::info!(
            operation = op,
            system = cmd
                .get("system")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            "Administration command completed"
        );
    }
    Ok(result)
}
pub fn admin_request(path: &Path, command: Value, max_response: usize) -> Result<Value> {
    use std::io::{BufRead, Write};
    let mut socket = std::os::unix::net::UnixStream::connect(path).with_context(|| {
        format!(
            "Cannot connect to {}; run the service and use an authorized local account",
            path.display()
        )
    })?;
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut input = serde_json::to_vec(&command)?;
    input.push(b'\n');
    socket.write_all(&input)?;
    let mut bytes = Vec::new();
    std::io::BufReader::new(std::io::Read::take(socket, max_response as u64 + 1))
        .read_until(b'\n', &mut bytes)?;
    if bytes.len() > max_response {
        bail!("Administration response exceeds configured state limit");
    }
    let output: Value = serde_json::from_slice(&bytes)?;
    if output.get("ok") != Some(&Value::Bool(true)) {
        bail!(
            "{}",
            output
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Administration command failed")
        );
    }
    Ok(output["result"].clone())
}

#[cfg(test)]
mod logging_tests {
    use super::*;

    #[test]
    fn message_logging_keeps_normal_details_and_bounds_large_utf8_strings() {
        assert_eq!(
            bounded_log_message(Some(&json!("all systems reachable"))),
            "all systems reachable"
        );
        let message = json!("€".repeat(10_000));
        let logged = bounded_log_message(Some(&message));
        assert!(logged.len() <= 16 * 1024 + "...[truncated]".len());
        assert!(logged.ends_with("...[truncated]"));
        assert_eq!(
            bounded_log_message(Some(&json!({"nested":"secret"}))),
            "[non-string message]"
        );
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[tokio::test]
    async fn live_socket_is_preserved_and_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let inode = fs::symlink_metadata(&path).unwrap().ino();

        let error = bind_socket(&path, 0o600).await.unwrap_err();

        assert!(error.to_string().contains("already in use"));
        assert_eq!(fs::symlink_metadata(&path).unwrap().ino(), inode);
        let _connection = UnixStream::connect(&path).await.unwrap();
        drop(listener);
    }

    #[tokio::test]
    async fn refused_stale_socket_is_rebound_with_requested_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stale.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());

        let listener = bind_socket(&path, 0o600).await.unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _connection = UnixStream::connect(&path).await.unwrap();
        drop(listener);
    }

    #[tokio::test]
    async fn existing_regular_file_is_never_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("protected.sock");
        fs::write(&path, b"preserve this file").unwrap();

        let error = bind_socket(&path, 0o600).await.unwrap_err();

        assert!(error.to_string().contains("Refusing to replace non-socket"));
        assert_eq!(fs::read(&path).unwrap(), b"preserve this file");
    }
}
