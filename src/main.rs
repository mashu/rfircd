//! rfircd - IRC server with an AX.25 packet radio gateway.
//!
//! Usage: `rfircd [--config path] [--check] [--hash-password]`

use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::Mutex;

use rfircd::cli;
use rfircd::config::Config;
use tracing::info;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (path, check_only) = match cli::parse_args(std::env::args().skip(1)) {
        cli::Invocation::Run { path } => (path, false),
        cli::Invocation::Check { path } => (path, true),
        cli::Invocation::Init { path } => return rfircd::wizard::run(&path),
        cli::Invocation::HashPassword => {
            use std::io::Read;
            let mut password = String::new();
            std::io::stdin().read_to_string(&mut password)?;
            let password = password.trim_end_matches(['\r', '\n']);
            if password.is_empty() {
                anyhow::bail!("no password on stdin");
            }
            println!(
                "{}",
                rfircd::accounts::hash_password(password)
                    .map_err(|_| anyhow::anyhow!("could not hash password"))?
            );
            return Ok(());
        }
        cli::Invocation::Version(text) => {
            println!("{text}");
            return Ok(());
        }
        cli::Invocation::Help(text) => {
            print!("{text}");
            return Ok(());
        }
        cli::Invocation::Usage(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let config = Config::load(&path)?;
    if check_only {
        println!("{path}: configuration is valid");
        return Ok(());
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "rfircd=info".into());
    let stdout = tracing_subscriber::fmt::layer();
    if let Some(log_path) = &config.logging.file {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(Mutex::new(file));
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout)
            .with(file_layer)
            .init();
        info!(path = %log_path, "logging to file");
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout)
            .init();
    }

    let config = Arc::new(config);

    let gateway = cli::build(config.clone())?;
    cli::spawn_listeners(
        &config,
        gateway.events.clone(),
        gateway.server.admission.clone(),
    )
    .await?;
    cli::spawn_shutdown(gateway.events.clone());
    cli::serve(gateway).await;
    Ok(())
}
