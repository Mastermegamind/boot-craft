//! The core of boot-craft: everything except the command line and the GUI.
//!
//! The modules at the top level are platform-independent: what a drive is
//! and whether it may be erased ([`drive`]), what an image is ([`image`]),
//! opening plain, compressed and VHD images ([`source`]), published
//! checksums ([`checksums`]), writing and verifying ([`write`]) and hashing
//! ([`hash`]). [`platform`]
//! holds the only OS-specific code: listing drives and opening one raw.

pub mod checksums;
pub mod detect;
pub mod drive;
pub mod error;
pub mod hash;
pub mod image;
pub mod partitions;
pub mod platform;
pub mod source;
pub mod tree;
pub mod write;

pub use drive::{Drive, Partition, Policy, Refusal, Transport, Usage, UsageKind};
pub use error::Error;
