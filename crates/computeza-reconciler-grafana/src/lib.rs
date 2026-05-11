//! Grafana reconciler.
//!
//! Manages BI and visualisation dashboards per spec section 7.11 -- datasources
//! auto-wired to GreptimeDB and Databend, dashboards-as-code, OIDC.
//! Stateless behind LB; backed by HA Postgres. Grafana is the only
//! non-Rust UI component; the spec notes we'll revisit when a
//! production-grade Rust BI replacement matures.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
