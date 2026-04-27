//! Kanidm reconciler.
//!
//! Manages user accounts, OAuth2 clients, groups, password / passkey
//! policies, and IdP federation per spec §7.1. Communicates over Kanidm
//! REST + LDAPS. HA is 3-node read replicas + write replica with manual
//! failover.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
