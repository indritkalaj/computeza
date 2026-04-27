//! Computeza tenancy — workspace isolation, quotas, billing metering.
//!
//! Per spec §3.6, the platform supports three tenancy models:
//! single-tenant (default), internal multi-tenant (Business tier and above),
//! and provider multi-tenant (Provider channel program — spec §11.6).
//!
//! Cryptographic separation, network isolation, resource quotas, per-tenant
//! metering, and tenant self-service all flow through this crate.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
