use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    pub cache: PathBuf,
    pub tokens: PathBuf,
    pub api_socket: PathBuf,
    pub admin_socket: PathBuf,
    pub max_state_bytes: usize,
    pub max_body_bytes: usize,
    pub cpu_count: usize,
    pub memory_mb: usize,
    pub max_connections: usize,
    pub log_requests: bool,
    pub log_queue_capacity: usize,
    pub poll_seconds: u64,
    pub service_name: String,
    pub public_base_url: Option<String>,
}
fn number(name: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    let value = match env::var(name) {
        Ok(s) => s.parse().with_context(|| format!("{name} must be an integer"))?,
        Err(env::VarError::NotPresent) => default,
        Err(e) => return Err(e.into()),
    };
    if !(min..=max).contains(&value) { bail!("{name} must be between {min} and {max}"); }
    Ok(value)
}
impl Config {
    pub fn from_env() -> Result<Self> {
        let memory_mb = number("SERVER_MEMORY_MB", 512, 128, 1_048_576)?;
        let cpu_count = number("SERVER_CPU_COUNT", 2, 1, 1024)?;
        let max_state_bytes = number("MAX_STATE_BYTES", memory_mb * 1024 * 1024 / 64, 1024, memory_mb * 1024 * 1024 / 16)?;
        let path = |key: &str, default: &str| PathBuf::from(env::var(key).unwrap_or_else(|_| default.into()));
        let log_requests = match env::var("LOG_NETWORK_REQUESTS").unwrap_or_else(|_| "true".into()).as_str() {
            "true" | "1" => true, "false" | "0" => false, _ => bail!("LOG_NETWORK_REQUESTS must be true or false"),
        };
        let service_name=env::var("SERVICE_NAME").unwrap_or_else(|_|"jcb-api.service".into());
        if service_name.is_empty() || !service_name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.@".contains(&c)) { bail!("Invalid SERVICE_NAME"); }
        let public_base_url=env::var("PUBLIC_BASE_URL").ok().filter(|s| !s.is_empty());
        if let Some(url)=&public_base_url {
            let parsed=reqwest::Url::parse(url).context("PUBLIC_BASE_URL must be an absolute HTTP(S) URL")?;
            if !["http","https"].contains(&parsed.scheme()) || parsed.host_str().is_none() || !parsed.username().is_empty() || parsed.password().is_some() || parsed.query().is_some() || parsed.fragment().is_some() || parsed.path()!="/" { bail!("PUBLIC_BASE_URL must contain only HTTP(S) scheme and host"); }
        }
        let config=Self {
            cache:path("CACHE_FILE_PATH","cache.json"), tokens:path("TOKENS_FILE_PATH","tokens.json"),
            api_socket:path("API_SOCKET_PATH","/run/jcb-api/api.sock"), admin_socket:path("ADMIN_SOCKET_PATH","/run/jcb-api/admin.sock"),
            max_state_bytes, max_body_bytes:number("MAX_CONTENT_LENGTH",1_048_576,1024,16_777_216)?,
            cpu_count,memory_mb,max_connections:number("MAX_CONNECTIONS",(memory_mb * 2).clamp(128,8192),16,65536)?,
            log_requests,log_queue_capacity:number("LOG_QUEUE_CAPACITY",4096,64,1_048_576)?,
            poll_seconds:number("POLL_INTERVAL_SECONDS",2,1,3600)? as u64, service_name,public_base_url,
        };
        if config.cache==config.tokens || config.api_socket==config.admin_socket { bail!("State files and sockets must have distinct paths"); }
        Ok(config)
    }
}
