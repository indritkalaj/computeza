//! Generated proto module re-exports. tonic-build writes the
//! Rust code into `$OUT_DIR/<package>.rs`; we re-export the
//! `computeza.channel_partner.v1` namespace here so callers get a
//! stable Rust path.
//!
//! `#[allow(missing_docs)]` covers the generated structs / enums --
//! they're documented in `proto/channel_partner.proto`; duplicating
//! the comments to the Rust side would drift on every proto edit.

#![allow(missing_docs)]

/// `computeza.channel_partner` package.
pub mod channel_partner {
    /// `v1` major version. Bumping the version means a new generated
    /// module side by side, never a breaking change to v1.
    pub mod v1 {
        tonic::include_proto!("computeza.channel_partner.v1");
    }
}
