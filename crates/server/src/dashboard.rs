//! The HTML dashboard: the page itself, the live update stream that keeps
//! it current, and the stylesheet.

use std::convert::Infallible;

use askama::Template;
use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::{Stream, StreamExt, stream};
use labwatch_core::{
    collectors::host_name,
    status::{Section, Status},
};

use crate::{AppState, section_stream};

/// One station on the board: a section plus the name people see.
struct Station {
    id: String,
    title: String,
    section: Section,
    /// A single big readout instead of a list, for stations with one fact.
    feature: Option<Feature>,
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

        let feature = match section.collector.as_str() {
            "vpn" => section
                .checks
                .first()
                .map(|check| Feature::from_vpn_detail(&check.detail)),
            _ => None,
        };

        Self {
            id: section.collector.clone(),
            title,
            section,
            feature,
        }
    }
}

/// The base status strip across the top of the page.
#[derive(Template)]
#[template(path = "banner.html")]
struct BannerTemplate {
    host: String,
    status: Status,
    headline: String,
    detail: String,
    stations: Vec<Station>,
}

impl BannerTemplate {
    fn new(sections: Vec<Section>) -> Self {
        let (status, headline, detail) = summarize(&sections);
        Self {
            host: host_name(),
            status,
            headline,
            detail,
            stations: sections.into_iter().map(Station::from).collect(),
        }
    }
}

/// One station card. Also rendered on its own for live updates.
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
    stations: Vec<Station>,
}

/// The plain-language summary for the banner: worst status, a headline,
/// and a second line naming what's wrong.
fn summarize(sections: &[Section]) -> (Status, String, String) {
    let status = sections
        .iter()
        .map(|s| s.status)
        .max()
        .unwrap_or(Status::Unknown);

    let waiting = sections.iter().filter(|s| s.checks.is_empty()).count();

    // Every check that's a warning or worse, across all sections.
    let problems: Vec<_> = sections
        .iter()
        .flat_map(|s| &s.checks)
        .filter(|c| c.status >= Status::Warning)
        .collect();

    match problems.as_slice() {
        [] if waiting == sections.len() => (
            status,
            "Starting up".into(),
            "Taking the first readings".into(),
        ),
        [] if waiting > 0 => (
            status,
            "Checking in".into(),
            format!("{waiting} station(s) still reporting"),
        ),
        [] => (
            status,
            "All systems normal".into(),
            "Every check is green".into(),
        ),
        // A slice pattern: matches exactly one element and names it.
        [only] => (
            status,
            format!("{}: {}", only.name, only.detail),
            "One check needs attention".into(),
        ),
        many => {
            let names: Vec<&str> = many.iter().map(|c| c.name.as_str()).collect();
            (
                status,
                format!("{} things need attention", many.len()),
                names.join(", "),
            )
        }
    }
}

fn render(template: &impl Template) -> Response {
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
    let banner = match BannerTemplate::new(sections.clone()).render() {
        Ok(html) => html,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };

    render(&IndexTemplate {
        version: labwatch_core::version(),
        banner,
        stations: sections.into_iter().map(Station::from).collect(),
    })
}

/// Live updates for htmx. Each published section becomes two events:
/// the station's new card (named after the collector, e.g. "host") and a
/// fresh banner ("summary"). htmx swaps each into the element whose
/// `sse-swap` attribute matches the event name.
pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let hub = state.hub.clone();

    let events = section_stream(state).flat_map(move |section| {
        let id = section.collector.clone();
        let station = Station::from(section);
        let card = StationTemplate { station: &station }.render();
        let banner = BannerTemplate::new(hub.sections()).render();

        // Collect whichever renders succeeded into a small stream of events.
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
        stream::iter(out.into_iter().map(Ok::<_, Infallible>))
    });

    Sse::new(events).keep_alive(KeepAlive::default())
}

/// The stylesheet, compiled into the binary with `include_str!`.
pub async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/app.css"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use labwatch_core::status::Check;

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
    fn all_green_is_normal() {
        let sections = [section("host", vec![Check::new("CPU", Status::Ok, "3%")])];
        let (status, headline, _) = summarize(&sections);
        assert_eq!(status, Status::Ok);
        assert_eq!(headline, "All systems normal");
    }

    #[test]
    fn one_problem_is_named_in_the_headline() {
        let sections = [section(
            "services",
            vec![
                Check::new("Jellyfin", Status::Ok, "up"),
                Check::new("Sonarr", Status::Critical, "unreachable"),
            ],
        )];
        let (status, headline, _) = summarize(&sections);
        assert_eq!(status, Status::Critical);
        assert_eq!(headline, "Sonarr: unreachable");
    }

    #[test]
    fn several_problems_are_counted() {
        let sections = [section(
            "services",
            vec![
                Check::new("Sonarr", Status::Critical, "unreachable"),
                Check::new("Radarr", Status::Warning, "slow"),
            ],
        )];
        let (_, headline, detail) = summarize(&sections);
        assert_eq!(headline, "2 things need attention");
        assert_eq!(detail, "Sonarr, Radarr");
    }
}
