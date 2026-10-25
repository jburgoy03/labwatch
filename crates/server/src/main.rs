use askama::Template;
use axum::{
    Router,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};

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

#[tokio::main]
async fn main() {
    let app = Router::new().route("/", get(index));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("failed to bind port 3000");

    println!("Labwatch listening on http://127.0.0.1:3000");
    axum::serve(listener, app).await.expect("server error");
}
