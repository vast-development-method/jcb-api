#![forbid(unsafe_code)]
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use jcb_api::{config::Config, documentation, service, updater};
use serde_json::{json, Value};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::Command,
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "JCB network availability service and local administration"
)]
struct Cli {
    /// Load deployment settings; existing environment values take precedence.
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Run the isolated API and local administration sockets.
    Serve,
    /// Inspect and remove dynamically registered network records.
    #[command(subcommand)]
    Network(NetworkCommand),
    /// Manage per-system webhook tokens over the private local socket.
    #[command(subcommand)]
    Token(TokenCommand),
    /// Reload externally edited cache/token files after validation.
    Reload,
    /// Check the running service through its private local socket.
    Check,
    /// Print the public network-only OpenAPI description.
    Openapi,
    /// Show the service journal (requires journal permissions).
    Logs {
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: u32,
    },
    /// Check and install a verified stable release, independently of serving.
    Update(updater::UpdateArgs),
    /// Verify an asset's exact checksum against its SHA256SUMS release manifest.
    ReleaseVerify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        asset: PathBuf,
    },
}
#[derive(Subcommand)]
enum NetworkCommand {
    List,
    Remove(RemoveArgs),
}
#[derive(Args)]
struct RemoveArgs {
    #[arg(long)]
    system: String,
    #[arg(long)]
    project: Option<String>,
    #[arg(long, requires = "project")]
    target: Option<String>,
    /// JSON integer or quoted string; bare nonnumeric strings are also accepted.
    #[arg(long, requires = "target")]
    id: Option<String>,
    /// Confirm deletion without an interactive prompt.
    #[arg(long)]
    yes: bool,
}
#[derive(Subcommand)]
enum TokenCommand {
    /// Generate a token and display it once. Store its value securely.
    Create {
        #[arg(long)]
        system: String,
    },
    /// List SHA-256 fingerprints, never the token values.
    List {
        #[arg(long)]
        system: Option<String>,
    },
    /// Revoke a token by its full fingerprint from token list.
    Revoke {
        #[arg(long)]
        system: String,
        #[arg(long)]
        fingerprint: String,
    },
}
fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(path) = cli.env_file {
        dotenvy::from_path(&path).with_context(|| format!("Cannot load {}", path.display()))?;
    } else {
        let _ = dotenvy::dotenv();
    }
    // These commands do not create a Tokio runtime or load service state.
    match cli.command {
        Commands::Update(args) => {
            println!("{}", updater::run(args)?);
            return Ok(());
        }
        Commands::ReleaseVerify { manifest, asset } => {
            updater::verify(&manifest, &asset)?;
            println!("Release checksum verified");
            return Ok(());
        }
        Commands::Openapi => {
            println!(
                "{}",
                serde_json::to_string_pretty(&documentation::openapi())?
            );
            return Ok(());
        }
        _ => {}
    }
    let config = Config::from_env()?;
    let request = |value| {
        service::admin_request(
            &config.admin_socket,
            value,
            config
                .max_state_bytes
                .saturating_mul(2)
                .saturating_add(65536),
        )
    };
    let result = match cli.command {
        Commands::Serve => {
            let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
                .buffered_lines_limit(config.log_queue_capacity)
                .lossy(true)
                .finish(io::stderr());
            let errors = writer.error_counter();
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into());
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(false)
                .with_target(false)
                .init();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(config.cpu_count)
                .max_blocking_threads(config.cpu_count.clamp(2, 8))
                .enable_all()
                .build()?;
            let outcome=runtime.block_on(async move {
                tokio::spawn(async move{let mut last=0;loop{tokio::time::sleep(Duration::from_secs(5)).await;let current=errors.dropped_lines();if current>last{eprintln!("WARN log buffer overflow: {} records dropped; increase LOG_QUEUE_CAPACITY or reduce traffic",current-last);last=current;}}});
                service::serve(config).await
            });
            runtime.shutdown_timeout(Duration::from_secs(30));
            drop(guard);
            return outcome;
        }
        Commands::Network(NetworkCommand::List) => request(json!({"op":"network.list"}))?,
        Commands::Network(NetworkCommand::Remove(args)) => {
            if !args.yes {
                if !io::stdin().is_terminal() {
                    bail!("Deletion requires --yes when not interactive");
                }
                eprint!(
                    "Remove system {:?}, project {:?}, target {:?}, id {:?}? Type yes: ",
                    args.system, args.project, args.target, args.id
                );
                io::stderr().flush()?;
                let mut confirmation = String::new();
                io::stdin().read_line(&mut confirmation)?;
                if confirmation.trim() != "yes" {
                    bail!("Deletion cancelled");
                }
            }
            let id = args
                .id
                .map(|text| serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)));
            request(
                json!({"op":"network.remove","system":args.system,"project":args.project,"target":args.target,"id":id}),
            )?
        }
        Commands::Token(TokenCommand::Create { system }) => {
            let result = request(json!({"op":"token.create","system":system}))?;
            println!(
                "{}",
                result["token"]
                    .as_str()
                    .context("Service did not return a token")?
            );
            return Ok(());
        }
        Commands::Token(TokenCommand::List { system }) => {
            request(json!({"op":"token.list","system":system}))?
        }
        Commands::Token(TokenCommand::Revoke {
            system,
            fingerprint,
        }) => request(json!({"op":"token.revoke","system":system,"fingerprint":fingerprint}))?,
        Commands::Reload => request(json!({"op":"reload"}))?,
        Commands::Check => request(json!({"op":"check"}))?,
        Commands::Logs { follow, lines } => {
            let mut command = Command::new("journalctl");
            command.args([
                "--unit",
                &config.service_name,
                "--no-pager",
                "--lines",
                &lines.to_string(),
            ]);
            if follow {
                command.arg("--follow");
            }
            if !command.status()?.success() {
                bail!("journalctl failed; check journal permissions");
            }
            return Ok(());
        }
        _ => unreachable!("handled before configuration"),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
