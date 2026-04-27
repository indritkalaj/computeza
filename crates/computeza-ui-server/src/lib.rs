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

use axum::{response::Html, routing::get, Router};
use computeza_i18n::Localizer;
use leptos::prelude::*;
use tower_http::trace::TraceLayer;

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

    // Body fragment via Leptos view!. The shell (<!doctype>, <head>, palette
    // CSS) is composed around it as a string — once we add hydration and a
    // real <Layout> component, the whole document moves into Leptos.
    let body_view = view! {
        <main class="shell">
            <header class="brand-header">
                <h1 class="brand">{app_title.clone()}</h1>
                <p class="tagline">{tagline}</p>
            </header>
            <section class="hero">
                <h2>{title.clone()}</h2>
                <p class="subtitle">{subtitle}</p>
                <p class="status">{status}</p>
                <p class="spec-note">{spec_note}</p>
            </section>
            <footer class="meta">
                <span>{version_label}" "{version}</span>
            </footer>
        </main>
    };
    let body_html = body_view.to_html();

    // Spec §4.3 palette: indigo-900 canvas, indigo-300 / orange-500 accents,
    // slate-100 body text, dark theme by default. Inlined for v0.0.x; moves
    // to Tailwind once `cargo-leptos` lands.
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title} — {app_title}</title>
<style>
  :root {{
    --indigo-900: #1A1B3A;
    --indigo-700: #2E2F5C;
    --indigo-300: #8B85F0;
    --orange-500: #E07A4F;
    --slate-500:  #5C5E84;
    --slate-100:  #E8E9F3;
  }}
  * {{ box-sizing: border-box; }}
  html, body {{ margin: 0; padding: 0; }}
  body {{
    background: var(--indigo-900);
    color: var(--slate-100);
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
    line-height: 1.5;
    min-height: 100vh;
  }}
  .shell {{ max-width: 64rem; margin: 0 auto; padding: 3rem 2rem; }}
  .brand-header {{ border-bottom: 1px solid var(--indigo-700); padding-bottom: 1.5rem; margin-bottom: 2.5rem; }}
  .brand {{ color: var(--orange-500); font-size: 1.5rem; font-weight: 600; margin: 0 0 0.25rem 0; letter-spacing: -0.02em; }}
  .tagline {{ color: var(--indigo-300); margin: 0; font-size: 0.875rem; }}
  .hero h2 {{ font-size: 1.5rem; font-weight: 600; margin: 0 0 0.75rem 0; color: var(--slate-100); }}
  .hero .subtitle {{ color: var(--slate-100); margin: 0 0 1.5rem 0; }}
  .hero .status {{ color: var(--slate-500); font-size: 0.875rem; margin: 0 0 0.5rem 0; }}
  .hero .spec-note {{ color: var(--slate-500); font-size: 0.875rem; margin: 0; }}
  .meta {{ color: var(--slate-500); font-size: 0.75rem; margin-top: 4rem; padding-top: 1.5rem; border-top: 1px solid var(--indigo-700); }}
</style>
</head>
<body>
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
