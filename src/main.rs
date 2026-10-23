//! Ferrocine: self-hosted media server.

use ferrocine::{app, config, store};

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Configuration file, or directory of *.yaml files.
    #[arg(
        short,
        long,
        env = "FERROCINE_CONFIG",
        default_value = "/etc/ferrocine",
        global = true
    )]
    config: PathBuf,
    /// Log format.
    #[arg(long, env = "FERROCINE_LOG_FORMAT", default_value = "text", value_parser = ["text", "json"], global = true)]
    log_format: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (default).
    Run,
    /// Validate the configuration and exit.
    Validate,
    /// Apply database migrations and exit.
    Migrate,
    /// Probe the local `/healthz`, for container health checks.
    Healthcheck {
        /// Address to probe; by default the configured listener (127.0.0.1:8096
        /// if the configuration cannot be read).
        #[arg(long)]
        addr: Option<String>,
    },
}

fn init_tracing(format: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("FERROCINE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    if format == "json" {
        builder.json().flatten_event(true).init();
    } else {
        builder.init();
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Healthcheck { addr } => healthcheck(&addr.unwrap_or_else(|| health_addr(&cli.config))),
        Command::Validate => match config::load(&cli.config) {
            Ok(c) => {
                println!("configuration is valid: {} libraries", c.libraries.len());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        cmd @ (Command::Run | Command::Migrate) => {
            init_tracing(&cli.log_format);
            let config = match config::load(&cli.config) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("{e}");
                    return ExitCode::FAILURE;
                }
            };
            if rustls::crypto::ring::default_provider().install_default().is_err() {
                tracing::debug!("a TLS crypto provider was already installed");
            }
            let runtime = match build_runtime(config.server.worker_threads) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "cannot start the async runtime");
                    return ExitCode::FAILURE;
                }
            };
            let result = runtime.block_on(async move {
                match cmd {
                    Command::Migrate => {
                        let store = store::Store::connect(&config.database).await?;
                        store.migrate().await?;
                        tracing::info!("migrations applied");
                        Ok(())
                    }
                    _ => app::run(cli.config, config).await,
                }
            });
            // Do not wait for lingering blocking tasks (file walks) on exit.
            runtime.shutdown_timeout(Duration::from_secs(5));
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!("{e:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn build_runtime(workers: usize) -> std::io::Result<tokio::runtime::Runtime> {
    let mut b = tokio::runtime::Builder::new_multi_thread();
    if workers > 0 {
        b.worker_threads(workers);
    }
    b.enable_all().thread_name("ferrocine").build()
}

/// Local address of the HTTP listener.
fn health_addr(path: &std::path::Path) -> String {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    let Ok(c) = config::load(path) else {
        return "127.0.0.1:8096".into();
    };
    let l = c.server.listen;
    let ip = match l.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, l.port()).to_string()
}

/// Minimal HTTP GET of /healthz, without any extra dependency.
fn healthcheck(addr: &str) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(_) => return ExitCode::FAILURE,
    };
    let ok = rt.block_on(async {
        let probe = async {
            let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
            s.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .await
                .ok()?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.ok()?;
            Some(buf.starts_with(b"HTTP/1.1 200") || buf.starts_with(b"HTTP/1.0 200"))
        };
        tokio::time::timeout(Duration::from_secs(5), probe)
            .await
            .ok()
            .flatten()
            .unwrap_or(false)
    });
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
