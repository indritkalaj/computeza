//! Channel-partner gRPC API skeleton.
//!
//! Implements the AGENTS.md product constraint that Computeza is
//! sellable through direct / reseller / sub-reseller channels and
//! resellers need a programmatic provisioning surface separate from
//! the operator-facing web console.
//!
//! v0.0.x ships:
//!
//! - Proto definitions ([`proto::channel_partner::v1`]) generated
//!   from `proto/channel_partner.proto` via tonic-build + protox.
//!   The build is host-protoc-free thanks to protox.
//!
//! - [`StubChannelPartner`] -- a placeholder implementation of the
//!   service trait that returns `Status::unimplemented` for every
//!   call but compiles + serves so an integration test can verify
//!   the transport works.
//!
//! - [`tls::load_server_tls`] -- mTLS scaffolding that loads the
//!   server cert + private key + client-CA bundle from PEM files.
//!   Returns a `tonic::transport::ServerTlsConfig` callers feed to
//!   `Server::builder().tls_config(...)`.
//!
//! What's deferred to v0.1+:
//!
//! - Real provisioning logic (each rpc shells into computeza-state).
//! - Per-tier authentication: parse the mTLS client cert CN, map it
//!   onto the reseller's chain entry from the active license.
//! - Telemetry pipeline (the customer's local aggregator that feeds
//!   `StreamTelemetry`).
//! - Binary wiring: the gRPC service runs on a port separate from
//!   the HTTP console (typical 8401 -> 8402 split).

#![warn(missing_docs)]

pub mod proto;
pub mod tls;

mod service;

pub use service::StubChannelPartner;

/// Re-export the generated server trait so callers don't have to
/// reach into [`proto`] directly.
pub use proto::channel_partner::v1::channel_partner_server::{
    ChannelPartner, ChannelPartnerServer,
};

/// Re-export the generated client struct.
pub use proto::channel_partner::v1::channel_partner_client::ChannelPartnerClient;
