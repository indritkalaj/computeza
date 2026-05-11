//! Computeza pipelines -- pipeline definition and compilation to Restate workflows.
//!
//! Per spec section 5, pipelines are stored as a domain-specific YAML schema in the
//! platform's state store. On save the YAML is validated, compiled into a
//! Restate workflow definition (each node becomes a Restate handler
//! invocation), registered with Restate's admin API, and wired to its
//! triggers (cron, event, manual). The dual-mode contract -- visual canvas
//! and YAML are both editable, and either reflects the other -- is
//! release-blocking and lives here.
//!
//! v1.5 adds the AI node category (Embed / Index / Retrieve / Generate /
//! Eval / Agent / Tool Call / Approval Gate) per spec section 6.9; those node
//! types live alongside the v1.0 categories.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
