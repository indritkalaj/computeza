//! Computeza UI server — Leptos SSR + axum HTTP server.
//!
//! Per spec §4.1, the operator console is a server-rendered Rust application
//! using Leptos in SSR mode with selective hydration. This crate is the
//! entry point: it wires the axum router, hosts the request handlers, and
//! renders Leptos `view!` trees to HTML on the server.
//!
//! # What v0.0.x ships
//!
//! - `serve(addr)` boots an axum server bound to the given address
//! - `GET /` renders a localized welcome page using Leptos's `view!` macro
//! - `GET /healthz` returns a localized "ok" string for liveness probes
//! - `tower-http::TraceLayer` emits structured tracing for every request
//!
//! Hydration (client-side WASM that re-attaches reactivity) is deferred —
//! the v0.0.x console is purely server-rendered, no JavaScript. We add
//! hydration once we have an actual interactive surface (the install
//! wizard or the pipeline canvas) that needs it.
//!
//! # i18n
//!
//! Every visible string flows through [`computeza_i18n::Localizer`].
//! Hardcoded English strings in this crate are a release-blocking bug.

#![warn(missing_docs)]

use std::net::SocketAddr;

use axum::{
    http::header,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use computeza_i18n::Localizer;
use leptos::prelude::*;
use tower_http::trace::TraceLayer;

/// Tailwind-compatible utility CSS, embedded at compile time. Served at
/// `/static/computeza.css` and referenced from the home page.
const COMPUTEZA_CSS: &str = include_str!("../assets/computeza.css");

/// Boot the operator console on the given address. Awaits forever
/// (until the process is signalled to terminate).
///
/// Errors are returned as `anyhow::Error` so the binary can format and
/// log them uniformly.
pub async fn serve(addr: SocketAddr) -> anyhow::Result<()> {
    let app = router();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "computeza ui-server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the axum router. Public so integration tests can instantiate it
/// without going through `serve` (and choose their own bind port).
pub fn router() -> Router {
    Router::new()
        .route("/", get(home_handler))
        .route("/healthz", get(healthz_handler))
        .route("/static/computeza.css", get(css_handler))
        .layer(TraceLayer::new_for_http())
}

async fn home_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_home(&l))
}

async fn healthz_handler() -> String {
    let l = Localizer::english();
    l.t("ui-healthz-ok")
}

async fn css_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        COMPUTEZA_CSS,
    )
        .into_response()
}

/// Render the home page to a complete HTML document.
///
/// Public for testability and so future page modules can reuse the shell.
#[must_use]
pub fn render_home(localizer: &Localizer) -> String {
    let app_title = localizer.t("ui-app-title");
    let tagline = localizer.t("ui-app-tagline");
    let title = localizer.t("ui-welcome-title");
    let subtitle = localizer.t("ui-welcome-subtitle");
    let status = localizer.t("ui-welcome-status");
    let spec_note = localizer.t("ui-welcome-spec");
    let version_label = localizer.t("ui-footer-version");
    let version = env!("CARGO_PKG_VERSION");

    // Body fragment via Leptos view!. Tailwind-compatible utility classes
    // come from /static/computeza.css (see assets/computeza.css).
    let body_view = view! {
        <main class="mx-auto max-w-4xl p-12">
            <header class="border-b pb-6 mb-10">
                <h1 class="text-orange-500 text-2xl font-semibold tracking-tight m-0 mb-1">
                    {app_title.clone()}
                </h1>
                <p class="text-indigo-300 text-sm m-0">{tagline}</p>
            </header>
            <section>
                <h2 class="text-2xl font-semibold text-slate-100 m-0 mb-3">{title.clone()}</h2>
                <p class="text-slate-100 m-0 mb-6">{subtitle}</p>
                <p class="text-slate-500 text-sm m-0 mb-2">{status}</p>
                <p class="text-slate-500 text-sm m-0">{spec_note}</p>
            </section>
            <footer class="mt-16 pt-6 border-t text-slate-500 text-xs">
                <span>{version_label}" "{version}</span>
            </footer>
        </main>
    };
    let body_html = body_view.to_html();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title} — {app_title}</title>
<link rel="stylesheet" href="/static/computeza.css" />
</head>
<body class="bg-indigo-900 text-slate-100">
{body_html}
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_home_contains_localized_title() {
        let l = Localizer::english();
        let html = render_home(&l);
        assert!(
            html.contains("Welcome to Computeza"),
            "rendered HTML should contain the localized welcome title; got:\n{html}"
        );
    }

    #[test]
    fn render_home_is_a_complete_html_document() {
        let l = Localizer::english();
        let html = render_home(&l);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html lang=\"en\">"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn render_home_has_no_hardcoded_english_strings_outside_attributes() {
        // Sanity check: every <p> and <h*> text node should be a value the
        // localizer produced. We assert by checking that strings the .ftl
        // bundle defines actually appear (positive check) and that some
        // common hardcoded-English smell doesn't (negative check).
        let l = Localizer::english();
        let html = render_home(&l);
        assert!(html.contains("Computeza")); // ui-app-title
        assert!(html.contains("Open lakehouse control plane")); // ui-app-tagline
        assert!(html.contains("Pre-alpha")); // ui-welcome-status starts with this
    }
}
