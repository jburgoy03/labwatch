use std::{
    collections::HashMap,
    convert::Infallible,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use askama::Template;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use clap::{Parser, Subcommand};
use futures_util::{Stream, StreamExt, future, stream};
use labwatch_core::{
    collectors::{
        CPU_WARMUP, Collector, DockerCollector, HostCollector, ServiceCollector, VpnCollector,
    },
    config::Config,
    history::{History, Point, Range, unix_now},
    hub::Hub,
    scheduler,
    status::{Section, Status},
};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

/// Everything the HTTP handlers need. axum gives each handler a clone,
/// which is cheap because both fields are handles to shared data.
#[derive(Clone)]
struct AppState {
    hub: Hub,
    history: History,
    /// Cancelled on Ctrl+C so long-lived SSE streams end and shutdown
    /// doesn't wait on them forever.
    shutdown: CancellationToken,
}

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
    let mut docker = config
        .docker
        .as_ref()
        .map(DockerCollector::new)
        .transpose()?;

    // `.await` here yields to the runtime instead of blocking the thread.
    tokio::time::sleep(CPU_WARMUP).await;

    let started = Instant::now();

    // Run all collectors at once; `try_join!` stops at the first error.
    let (host_section, services_section, docker_section, vpn_section) = tokio::try_join!(
        run(&mut host),
        run(&mut services),
        run_optional(docker.as_mut()),
        run_optional(vpn.as_mut()),
    )?;

    // stderr, so `--json` output on stdout stays valid JSON.
    eprintln!("collected in {} ms", started.elapsed().as_millis());

    let mut sections = vec![host_section, services_section];
    sections.extend(docker_section);
    sections.extend(vpn_section);

    if json {
        println!("{}", serde_json::to_string_pretty(&sections)?);
    } else {
        sections.iter().for_each(print_section);
    }

    Ok(())
}

/// Runs a collector only if it's configured.
async fn run_optional(collector: Option<&mut impl Collector>) -> Result<Option<Section>> {
    match collector {
        Some(collector) => run(collector).await.map(Some),
        None => Ok(None),
    }
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
    let secs = Duration::from_secs;
    let intervals = &config.intervals;

    let slot_count = 2 + usize::from(config.docker.is_some()) + usize::from(config.vpn.is_some());
    let hub = Hub::new(slot_count);
    let history = History::open(&config.storage.path).await?;
    let shutdown = CancellationToken::new();

    // Subscribe before starting collectors so these see the first run.
    tokio::spawn(log_changes(hub.clone()));
    tokio::spawn(record_history(hub.clone(), history.clone()));
    tokio::spawn(rollup_every_hour(history.clone()));

    // A closure that hands out 0, 1, 2, ... It's `mut` because it changes
    // the `slot` variable it captured (Rust calls this an `FnMut`).
    let mut slot = 0;
    let mut next_slot = || {
        slot += 1;
        slot - 1
    };

    // Each `hub.clone()` copies two handles (an Arc and a channel sender),
    // not the data. Every task shares the same store and channel.
    scheduler::spawn(
        HostCollector::new(config.host.mounts.clone()),
        secs(intervals.host_secs),
        next_slot(),
        hub.clone(),
    );
    scheduler::spawn(
        ServiceCollector::new(config.services.clone())?,
        secs(intervals.services_secs),
        next_slot(),
        hub.clone(),
    );
    if let Some(docker_config) = &config.docker {
        scheduler::spawn(
            DockerCollector::new(docker_config)?,
            secs(intervals.docker_secs),
            next_slot(),
            hub.clone(),
        );
    }
    if let Some(vpn_config) = &config.vpn {
        scheduler::spawn(
            VpnCollector::new(vpn_config.clone())?,
            secs(intervals.vpn_secs),
            next_slot(),
            hub.clone(),
        );
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/history", get(api_history))
        .route("/api/events", get(api_events))
        .with_state(AppState {
            hub,
            history,
            shutdown: shutdown.clone(),
        });

    let listener = tokio::net::TcpListener::bind(&config.server.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.server.bind))?;

    println!("Labwatch listening on http://{}", config.server.bind);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;

    println!("Shut down cleanly");
    Ok(())
}

/// Resolves when Ctrl+C is pressed, which tells axum to stop accepting
/// connections and finish in-flight requests. Cancelling the token first
/// ends every open SSE stream, so there's nothing left to wait for.
async fn shutdown_signal(shutdown: CancellationToken) {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for Ctrl+C");
    shutdown.cancel();
}

/// `State(state)` is an axum extractor: axum hands this handler the state
/// we registered with `.with_state(...)`, like constructor injection.
async fn api_status(State(state): State<AppState>) -> Json<Vec<Section>> {
    Json(state.hub.sections())
}

/// Query string for `/api/history?collector=host&check=CPU&range=24h`.
/// `range` is optional and defaults to 24h.
#[derive(Deserialize)]
struct HistoryQuery {
    collector: String,
    check: String,
    #[serde(default)]
    range: Range,
}

async fn api_history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<Point>>, (StatusCode, String)> {
    state
        .history
        .series(&query.collector, &query.check, query.range, unix_now())
        .await
        .map(Json)
        // Turn a database error into a 500 response instead of a panic.
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))
}

/// Server-Sent Events: the current sections right away, then each new one
/// as collectors publish it. The browser listens with `EventSource`.
///
/// The return type `impl Stream<...>` says "some stream of events" without
/// naming the long, generated type that the combinators below build up.
async fn api_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before taking the snapshot, so nothing published in
    // between can slip through the gap.
    let updates = BroadcastStream::new(state.hub.subscribe())
        // A `Lagged` error just means we skipped some old updates; drop it.
        .filter_map(|message| future::ready(message.ok()));

    let snapshot = stream::iter(state.hub.sections());

    let events = snapshot
        .chain(updates)
        .filter_map(|section| {
            future::ready(Event::default().event("section").json_data(&section).ok())
        })
        // SSE streams carry `Result`s; ours can't fail, hence `Infallible`.
        .map(Ok::<_, Infallible>)
        // End the stream when shutdown starts.
        .take_until(state.shutdown.cancelled_owned());

    // Keep-alive comments stop proxies from closing an idle connection.
    Sse::new(events).keep_alive(KeepAlive::default())
}

/// Saves every published section to the database.
async fn record_history(hub: Hub, history: History) {
    let mut updates = hub.subscribe();

    loop {
        match updates.recv().await {
            Ok(section) => {
                if let Err(err) = history.record(unix_now(), &section).await {
                    eprintln!("failed to record {}: {err:#}", section.collector);
                }
            }
            Err(RecvError::Lagged(skipped)) => {
                eprintln!("history skipped {skipped} updates");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// Rolls samples up into hourly averages and prunes old data, once an hour.
async fn rollup_every_hour(history: History) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60 * 60));

    loop {
        // The first tick fires immediately, so a restart catches up right away.
        ticker.tick().await;
        if let Err(err) = history.rollup(unix_now()).await {
            eprintln!("history rollup failed: {err:#}");
        }
    }
}

/// Subscribes to the hub and prints a line whenever a collector's overall
/// status changes, e.g. `[services] OK -> CRIT`.
async fn log_changes(hub: Hub) {
    let mut updates = hub.subscribe();
    let mut last_seen: HashMap<String, Status> = HashMap::new();

    loop {
        match updates.recv().await {
            Ok(section) => {
                // `insert` returns the previous value, if there was one.
                match last_seen.insert(section.collector.clone(), section.status) {
                    None => println!("[{}] first run: {}", section.collector, section.status),
                    Some(previous) if previous != section.status => {
                        println!("[{}] {previous} -> {}", section.collector, section.status);
                    }
                    Some(_) => {} // unchanged, stay quiet
                }
            }
            // We fell behind and the channel dropped old messages. Not fatal:
            // the next message is still current.
            Err(RecvError::Lagged(skipped)) => eprintln!("logger skipped {skipped} updates"),
            // Every sender is gone, so the program is shutting down.
            Err(RecvError::Closed) => break,
        }
    }
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
