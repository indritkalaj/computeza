//! Databend reconciler.
//!
//! Manages the columnar SQL engine (vector + FTS + geospatial) per spec
//! §7.6. Communicates over the Databend admin API + MySQL/HTTP/CH handlers.
//! Stateless query nodes; Raft-replicated meta service.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
