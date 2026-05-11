//! Computeza UI themes -- brand themes and white-label support.
//!
//! Per spec section 11.6, Provider channel partners get white-label rights:
//! partner's logo, palette, domain, email branding. This crate hosts the
//! theme definition format (a small TOML schema mapping the standard
//! `indigo-900` ... `slate-100` token names from spec section 4.3 to partner
//! overrides) and the runtime that applies a theme to the Tailwind
//! CSS variable layer.
//!
//! The default theme is the indigo + orange palette from spec section 4.3; any
//! override theme MUST conform to the same token shape so existing
//! components render correctly without per-component theme awareness.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
