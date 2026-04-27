//! Computeza UI pipelines — drag-and-drop pipeline canvas.
//!
//! Per spec §5, this is the single largest UX surface in the product. Two
//! non-negotiable contracts:
//!
//! 1. **Dual-mode equivalence.** The canvas is one face of a single
//!    underlying YAML pipeline definition; the YAML is the other. Edit
//!    either; the other reflects. A change that doesn't reflect is a
//!    release-blocking bug.
//! 2. **Pre-built nodes only.** Users compose from a fixed palette of
//!    Source / Transform / Filter / Join / Sink / Control / Quality / AI
//!    nodes. Users do not author node *types* in the canvas — those are
//!    Rust SDK or YAML CDK artifacts (spec §5.8).
//!
//! Layout, palette, inspector tabs (Properties / Code / Sample), edge
//! type-checking, auto-layout (Sugiyama), and run history visualisations
//! all live in this crate.
//!
//! Scaffold stub. Implementation is pending. Note: spec §4.1 acknowledges
//! "no mature Rust node-graph library exists" — this canvas is custom-built.

#![warn(missing_docs)]
