//! The HTML dashboard: the page itself, the live update stream that keeps
//! it current, the stylesheet and the status-coloured tab icon.

use std::convert::Infallible;

use askama::Template;
use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::{Stream, StreamExt, stream};
use labwatch_core::{
    collectors::{host_name, thresholds, uptime_secs},
    history::unix_now,
    status::{Check, Section, Status},
};
use serde::Deserialize;

use crate::{AppState, section_stream, timeline};

/// One station on the board: a section plus how to draw it.
struct Station {
    id: String,
    title: String,
    section: Section,
    /// "meters" (the server), "readout" (the VPN) or "windows" (everything
    /// else: one annunciator window per check). A plain string because the
    /// template compares it directly.
    kind: &'static str,
    /// Filled for "meters" stations.
    meters: Vec<Meter>,
    /// Filled for "readout" stations: one big value with a caption.
    feature: Option<Feature>,
    /// For "windows" stations: "All 15 normal" or "2 of 15 need attention".
    count: String,
}

/// A large value with a caption under it, e.g. an IP and its location.
struct Feature {
    value: String,
    caption: String,
}

impl Feature {
    /// Splits a VPN detail like "203.0.113.7 (Chicago, United States)" into
    /// the IP and the place. Error details have no parentheses, so the
    /// whole message becomes the value.
    fn from_vpn_detail(detail: &str) -> Self {
        // `split_once` returns `Option<(&str, &str)>`: both halves, or None.
        match detail.split_once(" (") {
            Some((ip, place)) => Self {
                value: ip.to_string(),
                caption: place.trim_end_matches(')').to_string(),
            },
            None => Self {
                value: detail.to_string(),
                caption: String::new(),
            },
        }
    }
}

/// One host reading drawn as a panel meter.
struct Meter {
    name: String,
    detail: String,
    status: Status,
    /// `None` when there's no number to draw, e.g. a disk that isn't mounted.
    scale: Option<Scale>,
}

/// Positions on a 0–100 scale, as percentages of the meter's width.
struct Scale {
    value: f64,
    warn: f64,
    /// Width of the amber zone, from `warn` up to the critical threshold.
    warn_width: f64,
    crit: f64,
}

impl Meter {
    fn from_check(check: &Check) -> Self {
        // Both a value and known thresholds are needed to draw a scale.
        // `zip` pairs two Options: Some only when both are Some.
        let scale = check
            .value
            .zip(thresholds(&check.name))
            .map(|(value, (warn, crit))| Scale {
                value: value.clamp(0.0, 100.0),
                warn,
                warn_width: crit - warn,
                crit,
            });

        Self {
            name: check.name.clone(),
            detail: check.detail.clone(),
            status: check.status,
            scale,
        }
    }
}

impl From<Section> for Station {
    /// `From` is Rust's conversion trait; implementing it also gives you
    /// `section.into()` for free.
    fn from(section: Section) -> Self {
        let title = match section.collector.as_str() {
            "host" => "Server",
            "services" => "Services",
            "containers" => "Containers",
            "vpn" => "VPN tunnel",
            other => other,
        }
        .to_string();

        let kind = match section.collector.as_str() {
            "host" => "meters",
            "vpn" => "readout",
            _ => "windows",
        };

        let meters = match kind {
            "meters" => section.checks.iter().map(Meter::from_check).collect(),
            _ => Vec::new(),
        };

        let feature = match kind {
            "readout" => section
                .checks
                .first()
                .map(|check| Feature::from_vpn_detail(&check.detail)),
            _ => None,
        };

        Self {
            id: section.collector.clone(),
            count: count_line(&section.checks),
            title,
            kind,
            meters,
            feature,
            section,
        }
    }
}

/// "All 15 normal", or "2 of 15 need attention".
fn count_line(checks: &[Check]) -> String {
    let total = checks.len();
    let problems = checks
        .iter()
        .filter(|c| c.status >= Status::Warning)
        .count();
    match (problems, total) {
        (0, 1) => "Normal".into(),
        (0, n) => format!("All {n} normal"),
        (p, n) => format!("{p} of {n} need attention"),
    }
}

/// The status header across the top of the page.
#[derive(Template)]
#[template(path = "banner.html")]
struct BannerTemplate {
    host: String,
    uptime: String,
    status: Status,
    headline: String,
    detail: String,
    /// The browser tab's title, kept in sync by a few lines of script.
    tab_title: String,
}

impl BannerTemplate {
    fn new(sections: &[Section]) -> Self {
        let (status, headline, detail) = summarize(sections);
        let tab_title = match status {
            Status::Ok => "Labwatch".to_string(),
            _ => format!("{headline} · Labwatch"),
        };
        Self {
            host: host_name(),
            uptime: format_uptime(uptime_secs()),
            status,
            headline,
            detail,
            tab_title,
        }
    }
}

/// One station. Also rendered on its own for live updates.
#[derive(Template)]
#[template(path = "station.html")]
struct StationTemplate<'a> {
    station: &'a Station,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    version: &'static str,
    banner: String,
    /// The 24-hour incident strip, or empty if it couldn't be built.
    timeline: String,
    /// Overall status, for the first tab icon before any live update.
    status: Status,
    tab_title: String,
    /// The narrow column: server meters and the VPN readout.
    side: Vec<Station>,
    /// The wide column: annunciator panels.
    main: Vec<Station>,
}

/// "Server up 21 hours", "Server up 3 days", "Server up 40 minutes".
fn format_uptime(secs: u64) -> String {
    let (minutes, hours, days) = (secs / 60, secs / 3600, secs / 86_400);
    let (n, unit) = match (days, hours, minutes) {
        (d, _, _) if d >= 2 => (d, "day"),
        (_, h, _) if h >= 1 => (h, "hour"),
        (_, _, m) => (m.max(1), "minute"),
    };
    let s = if n == 1 { "" } else { "s" };
    format!("Server up {n} {unit}{s}")
}

/// "4 services", "1 container".
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Uppercases the first letter: "unreachable" becomes "Unreachable".
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The plain-language summary for the header: worst status, a headline
/// and a second line.
fn summarize(sections: &[Section]) -> (Status, String, String) {
    let status = sections
        .iter()
        .map(|s| s.status)
        .max()
        .unwrap_or(Status::Unknown);

    let waiting = sections.iter().filter(|s| s.checks.is_empty()).count();

    // Every check that's a warning or worse, worst first. `sort_by_key`
    // is stable, so equal statuses keep the board's order.
    let mut problems: Vec<(&str, &Check)> = sections
        .iter()
        .flat_map(|s| s.checks.iter().map(move |c| (s.collector.as_str(), c)))
        .filter(|(_, c)| c.status >= Status::Warning)
        .collect();
    problems.sort_by_key(|(_, c)| std::cmp::Reverse(c.status));

    // A slice pattern: `[worst, rest @ ..]` names the first element and the
    // remainder in one go.
    match problems.as_slice() {
        [] if waiting == sections.len() => (
            status,
            "Starting up".into(),
            "Taking the first readings".into(),
        ),
        [] if waiting > 0 => (
            status,
            "Checking in".into(),
            format!("{} still reporting", plural(waiting, "station")),
        ),
        [] => (status, "All good".into(), all_good_line(sections)),
        [(collector, worst), rest @ ..] => {
            let headline = match (worst.status, *collector) {
                (Status::Critical, "services" | "containers") => format!("{} is down", worst.name),
                (Status::Critical, _) => format!("{} needs attention now", worst.name),
                _ => format!("{} needs attention", worst.name),
            };
            let mut detail = sentence(&worst.detail);
            if !rest.is_empty() {
                let names: Vec<&str> = rest.iter().map(|(_, c)| c.name.as_str()).collect();
                let verb = if rest.len() == 1 { "needs" } else { "need" };
                detail = format!(
                    "{detail}. {} more {verb} attention: {}.",
                    rest.len(),
                    names.join(", ")
                );
            }
            (status, headline, detail)
        }
    }
}

/// "4 services and 15 containers running. Traffic leaves home through Chicago."
fn all_good_line(sections: &[Section]) -> String {
    let count = |collector: &str| {
        sections
            .iter()
            .find(|s| s.collector == collector)
            .map(|s| s.checks.len())
            .filter(|&n| n > 0)
    };

    let running = match (count("services"), count("containers")) {
        (Some(s), Some(c)) => Some(format!(
            "{} and {} running.",
            plural(s, "service"),
            plural(c, "container")
        )),
        (Some(s), None) => Some(format!("{} running.", plural(s, "service"))),
        (None, Some(c)) => Some(format!("{} running.", plural(c, "container"))),
        (None, None) => None,
    };

    // The city is the part of "Chicago, United States" before the comma.
    let exit = sections
        .iter()
        .find(|s| s.collector == "vpn")
        .and_then(|s| s.checks.first())
        .map(|c| Feature::from_vpn_detail(&c.detail).caption)
        .and_then(|place| place.split(',').next().map(str::to_string))
        .filter(|city| !city.is_empty())
        .map(|city| format!("Traffic leaves home through {city}."));

    // Keep whichever parts exist; fall back when neither does.
    let parts: Vec<String> = running.into_iter().chain(exit).collect();
    if parts.is_empty() {
        "Every check is green.".into()
    } else {
        parts.join(" ")
    }
}

pub(crate) fn render(template: &impl Template) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {err}"),
        )
            .into_response(),
    }
}

pub async fn index(State(state): State<AppState>) -> Response {
    let sections = state.hub.sections();
    let banner_template = BannerTemplate::new(&sections);
    let (status, tab_title) = (banner_template.status, banner_template.tab_title.clone());
    let banner = match banner_template.render() {
        Ok(html) => html,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };

    let timeline = timeline::render_strip(&state.history, unix_now())
        .await
        .unwrap_or_default();

    // `partition` splits one list into two by a test, in one pass.
    let (side, main): (Vec<Station>, Vec<Station>) = sections
        .into_iter()
        .map(Station::from)
        .partition(|station| station.kind != "windows");

    render(&IndexTemplate {
        version: labwatch_core::version(),
        banner,
        timeline,
        status,
        tab_title,
        side,
        main,
    })
}

/// Live updates for htmx. Each published section becomes three events:
/// the station's new markup (named after the collector, e.g. "host"), a
/// fresh header ("summary") and the incident strip ("timeline"). htmx swaps
/// each into the element whose `sse-swap` attribute matches the event name.
pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let hub = state.hub.clone();
    let history = state.history.clone();

    // `then` runs an async step per item, like `Select` with an `await`
    // inside. The timeline needs one: it reads incidents from SQLite.
    let events = section_stream(state)
        .then(move |section| {
            // Each call gets its own handles, because the async block below
            // may outlive this call. Cloning is cheap: both are shared handles.
            let hub = hub.clone();
            let history = history.clone();
            async move {
                let id = section.collector.clone();
                let station = Station::from(section);
                let card = StationTemplate { station: &station }.render();
                let banner = BannerTemplate::new(&hub.sections()).render();
                let strip = timeline::render_strip(&history, unix_now()).await;

                // Collect whichever renders succeeded into a few events.
                // SSE data can't contain raw newlines, and HTML doesn't need them.
                let mut out = Vec::new();
                if let Ok(html) = card {
                    out.push(Event::default().event(id).data(html.replace('\n', "")));
                }
                if let Ok(html) = banner {
                    out.push(
                        Event::default()
                            .event("summary")
                            .data(html.replace('\n', "")),
                    );
                }
                if let Some(html) = strip {
                    out.push(
                        Event::default()
                            .event("timeline")
                            .data(html.replace('\n', "")),
                    );
                }
                out
            }
        })
        // Flatten each batch of events into the one outgoing stream.
        .flat_map(|out| stream::iter(out.into_iter().map(Ok::<_, Infallible>)));

    Sse::new(events).keep_alive(KeepAlive::default())
}

/// The stylesheet, compiled into the binary with `include_str!`.
pub async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/app.css"),
    )
}

/// Query string for `/favicon.svg?status=critical`.
#[derive(Deserialize)]
pub struct IconQuery {
    status: Option<String>,
}

/// The tab icon: the header's lamp, coloured by status. The page swaps the
/// `status` in the icon's URL whenever the header updates.
pub async fn favicon(Query(query): Query<IconQuery>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            // Each status is its own URL, so caching never shows a stale colour.
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        favicon_svg(query.status.as_deref()),
    )
}

fn favicon_svg(status: Option<&str>) -> String {
    let lamp = match status {
        Some("ok") => "#3DBE6E",
        Some("warning") => "#FFB627",
        Some("critical") => "#E5482F",
        // Unknown, or the plain icon used on other pages.
        _ => "#8A9096",
    };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#2B3035"/><circle cx="16" cy="16" r="11.5" fill="#B5BAB3"/><circle cx="16" cy="16" r="8.5" fill="{lamp}"/><ellipse cx="13.2" cy="12.8" rx="3" ry="2.2" fill="#fff" opacity=".5"/></svg>"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(name: &str, checks: Vec<Check>) -> Section {
        Section::new(name, checks)
    }

    #[test]
    fn vpn_detail_splits_into_ip_and_place() {
        let feature = Feature::from_vpn_detail("203.0.113.7 (Chicago, United States)");
        assert_eq!(feature.value, "203.0.113.7");
        assert_eq!(feature.caption, "Chicago, United States");
    }

    #[test]
    fn vpn_error_detail_stays_whole() {
        let feature = Feature::from_vpn_detail("control server unreachable");
        assert_eq!(feature.value, "control server unreachable");
        assert!(feature.caption.is_empty());
    }

    #[test]
    fn all_pending_is_starting_up() {
        let (_, headline, _) = summarize(&[Section::pending("host")]);
        assert_eq!(headline, "Starting up");
    }

    #[test]
    fn all_green_says_what_is_running() {
        let sections = [
            section(
                "services",
                vec![Check::new("Jellyfin", Status::Ok, "up, 5 ms")],
            ),
            section(
                "containers",
                vec![
                    Check::new("jellyfin", Status::Ok, "Up 2 weeks"),
                    Check::new("sonarr", Status::Ok, "Up 2 weeks"),
                ],
            ),
            section(
                "vpn",
                vec![Check::new(
                    "Exit IP",
                    Status::Ok,
                    "203.0.113.7 (Chicago, United States)",
                )],
            ),
        ];
        let (status, headline, detail) = summarize(&sections);
        assert_eq!(status, Status::Ok);
        assert_eq!(headline, "All good");
        assert_eq!(
            detail,
            "1 service and 2 containers running. Traffic leaves home through Chicago."
        );
    }

    #[test]
    fn all_green_without_services_falls_back() {
        let sections = [section("host", vec![Check::new("CPU", Status::Ok, "3%")])];
        let (_, headline, detail) = summarize(&sections);
        assert_eq!(headline, "All good");
        assert_eq!(detail, "Every check is green.");
    }

    #[test]
    fn a_down_service_leads_the_headline() {
        let sections = [section(
            "services",
            vec![
                Check::new("Radarr", Status::Ok, "up"),
                Check::new("Jellyfin", Status::Critical, "unreachable"),
            ],
        )];
        let (status, headline, detail) = summarize(&sections);
        assert_eq!(status, Status::Critical);
        assert_eq!(headline, "Jellyfin is down");
        assert_eq!(detail, "Unreachable");
    }

    #[test]
    fn worst_problem_leads_and_the_rest_are_named() {
        let sections = [
            section(
                "services",
                vec![Check::new("Sonarr", Status::Warning, "HTTP 503, 40 ms")],
            ),
            section(
                "containers",
                vec![Check::new("jellyfin", Status::Critical, "Exited (137)")],
            ),
        ];
        let (_, headline, detail) = summarize(&sections);
        assert_eq!(headline, "jellyfin is down");
        assert_eq!(detail, "Exited (137). 1 more needs attention: Sonarr.");
    }

    #[test]
    fn host_problems_are_not_called_down() {
        let sections = [section(
            "host",
            vec![Check::new("CPU", Status::Critical, "97.0% used")],
        )];
        let (_, headline, _) = summarize(&sections);
        assert_eq!(headline, "CPU needs attention now");
    }

    #[test]
    fn count_lines() {
        let ok = Check::new("a", Status::Ok, "");
        let bad = Check::new("b", Status::Critical, "");
        assert_eq!(count_line(std::slice::from_ref(&ok)), "Normal");
        assert_eq!(count_line(&[ok.clone(), ok.clone()]), "All 2 normal");
        assert_eq!(count_line(&[ok, bad]), "1 of 2 need attention");
    }

    #[test]
    fn uptime_reads_naturally() {
        assert_eq!(format_uptime(30), "Server up 1 minute");
        assert_eq!(format_uptime(40 * 60), "Server up 40 minutes");
        assert_eq!(format_uptime(3600), "Server up 1 hour");
        assert_eq!(format_uptime(21 * 3600), "Server up 21 hours");
        assert_eq!(format_uptime(30 * 3600), "Server up 30 hours");
        assert_eq!(format_uptime(3 * 86_400), "Server up 3 days");
    }

    #[test]
    fn meters_get_scales_only_with_a_value() {
        let cpu = Meter::from_check(&Check::new("CPU", Status::Ok, "2%").with_value(2.4));
        let scale = cpu.scale.expect("CPU has a value and thresholds");
        assert_eq!((scale.warn, scale.crit), (80.0, 95.0));
        assert_eq!(scale.warn_width, 15.0);

        let unmounted = Meter::from_check(&Check::new(
            "Disk /mnt/media",
            Status::Critical,
            "not mounted",
        ));
        assert!(unmounted.scale.is_none());
    }

    #[test]
    fn favicon_colour_follows_status() {
        assert!(favicon_svg(Some("critical")).contains("#E5482F"));
        assert!(favicon_svg(Some("ok")).contains("#3DBE6E"));
        assert!(favicon_svg(None).contains("#8A9096"));
    }
}
