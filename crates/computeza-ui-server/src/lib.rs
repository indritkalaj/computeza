//! Computeza UI server — Leptos SSR entry point.
//!
//! Per spec §4.1, the web console is a server-rendered Rust application
//! using Leptos in SSR mode with selective hydration. axum hosts it; tokio
//! is the async runtime; Tailwind CSS provides the utility-first styling
//! described in spec §4.3.
//!
//! This crate wires the routing, layouts, auth middleware, and WebSocket
//! channel for live reconciliation events. Page modules live in
//! `computeza-ui-pages`; reusable widgets in `computeza-ui-components`;
//! the pipeline canvas in `computeza-ui-pipelines`.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
