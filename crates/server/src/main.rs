use std::{path::PathBuf, time::Instant};

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
    status::Section,
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
    Snapshot {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Command::Serve => serve(&config).await,
        Command::Snapshot { json } => snapshot(&config, json).await,
    }
}

async fn snapshot(config: &Config, json: bool) -> Result<()> {
    let mut host = HostCollector::new(config.host.mounts.clone());
    let mut services = ServiceCollector::new(config.services.clone())?;

    // `Option<VpnConfig>` -> `Option<Result<VpnCollector>>` -> `Result<Option<VpnCollector>>`.
    // `transpose` flips the two so `?` can catch a construction error.
    let mut vpn = config.vpn.clone().map(VpnCollector::new).transpose()?;

    // `.await` here yields to the runtime instead of blocking the thread.
    tokio::time::sleep(CPU_WARMUP).await;

    let started = Instant::now();

    // An async block is a future we can hand to `try_join!` like any other.
    let vpn_section = async {
        match vpn.as_mut() {
            Some(vpn) => run(vpn).await.map(Some),
            None => Ok(None),
        }
    };

    // Run all collectors at once; `try_join!` stops at the first error.
    let (host_section, services_section, vpn_section) =
        tokio::try_join!(run(&mut host), run(&mut services), vpn_section)?;

    // stderr, so `--json` output on stdout stays valid JSON.
    eprintln!("collected in {} ms", started.elapsed().as_millis());

    let mut sections = vec![host_section, services_section];
    sections.extend(vpn_section);

    if json {
        println!("{}", serde_json::to_string_pretty(&sections)?);
    } else {
        sections.iter().for_each(print_section);
    }

    Ok(())
}

/// Runs any collector and wraps its checks in a `Section`.
///
/// `impl Collector` means "any type that implements the trait". The compiler
/// generates a copy of this function per collector type, like a C# generic
/// method with a `where T : ICollector` constraint.
async fn run(collector: &mut impl Collector) -> Result<Section> {
    let checks = collector.collect().await?;
    Ok(Section::new(collector.name(), checks))
}

fn print_section(section: &Section) {
    println!("{} [{}]", section.collector, section.status);
    for check in &section.checks {
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
