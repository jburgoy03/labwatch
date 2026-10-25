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

// No `#[tokio::main]` here: reqwest's blocking client starts its own
// runtime and panics if called from inside another one. So only `serve`
// builds a tokio runtime. In Phase 2 everything goes async and this
// goes back to `#[tokio::main]`.
fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Command::Serve => tokio::runtime::Runtime::new()?.block_on(serve(&config)),
        Command::Snapshot { json } => snapshot(&config, json),
    }
}

fn snapshot(config: &Config, json: bool) -> Result<()> {
    let mut host = HostCollector::new(config.host.mounts.clone());
    std::thread::sleep(CPU_WARMUP);

    let mut sections = vec![
        run(&mut host)?,
        run(&mut ServiceCollector::new(config.services.clone())?)?,
    ];

    // `if let` runs the block only when `[vpn]` is configured.
    if let Some(vpn_config) = &config.vpn {
        sections.push(run(&mut VpnCollector::new(vpn_config.clone())?)?);
    }

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
fn run(collector: &mut impl Collector) -> Result<Section> {
    let checks = collector.collect()?;
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
