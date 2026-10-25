use std::path::PathBuf;

use anyhow::{Context, Result};
use askama::Template;
use axum::{
    Router,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use clap::{Parser, Subcommand};
use labwatch_core::{
    collectors::{CPU_WARMUP, Collector, HostCollector, ServiceCollector, VpnCollector},
    config::Config,
    status::Check,
};

/// Homelab status dashboard.
///
/// clap turns this struct into the argument parser. Doc comments become
/// the `--help` text.
#[derive(Parser)]
#[command(name = "labwatch", version)]
struct Cli {
    /// Path to the config file.
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

/// Each enum variant is one subcommand.
#[derive(Subcommand)]
enum Command {
    /// Run the web dashboard.
    Serve,
    /// Collect once, print the results and exit.
    Snapshot,
}

// No `#[tokio::main]` here: reqwest's blocking client starts its own
// runtime and panics if called from inside another one. So only `serve`
// builds a tokio runtime. In Phase 2 everything goes async and this
// goes back to `#[tokio::main]`.
fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Command::Serve => tokio::runtime::Runtime::new()?.block_on(serve(&config)),
        Command::Snapshot => snapshot(&config),
    }
}

fn snapshot(config: &Config) -> Result<()> {
    let mut host = HostCollector::new(config.host.mounts.clone());
    std::thread::sleep(CPU_WARMUP);

    let checks = host.collect()?;
    print_section(host.name(), &checks);

    let mut services = ServiceCollector::new(config.services.clone())?;
    let checks = services.collect()?;
    print_section(services.name(), &checks);

    // `if let` runs the block only when `[vpn]` is configured.
    if let Some(vpn_config) = &config.vpn {
        let mut vpn = VpnCollector::new(vpn_config.clone())?;
        let checks = vpn.collect()?;
        print_section(vpn.name(), &checks);
    }

    Ok(())
}

fn print_section(name: &str, checks: &[Check]) {
    println!("{name}");
    for check in checks {
        println!("  {:<7} {:<22} {}", check.status, check.name, check.detail);
    }
}

async fn serve(config: &Config) -> Result<()> {
    let app = Router::new().route("/", get(index));

    let listener = tokio::net::TcpListener::bind(&config.server.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.server.bind))?;

    println!("Labwatch listening on http://{}", config.server.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

/// The home page. Askama checks `templates/index.html` against this
/// struct at compile time, so a typo in a template variable is a build error.
#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    version: &'static str,
}

async fn index() -> Response {
    let page = IndexTemplate {
        version: labwatch_core::version(),
    };

    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {err}"),
        )
            .into_response(),
    }
}
