//! Apache XTable reconciler.
//!
//! Manages translation jobs for Iceberg <-> Delta <-> Hudi metadata sync per
//! spec section 7.5. XTable is the only managed component that is not Rust-native;
//! it runs on the bundled OpenJDK 21 JRE in a sidecar process with strict
//! resource limits (spec section 10.4). v1.6 targets a Rust-native replacement
//! that eliminates the bundled JRE (spec section 13.3).
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
