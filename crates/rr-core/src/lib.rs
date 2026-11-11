//! The core of rust-rufus: everything except the command line and the GUI.
//!
//! The modules at the top level are platform-independent: what a drive is
//! and whether it may be erased ([`drive`]), what an image is ([`image`]),
//! writing and verifying ([`write`]) and hashing ([`hash`]). [`platform`]
//! holds the only OS-specific code: listing drives and opening one raw.

pub mod drive;
pub mod error;
pub mod hash;
pub mod image;
pub mod platform;
pub mod write;

pub use drive::{Drive, Partition, Policy, Refusal, Transport, Usage, UsageKind};
pub use error::Error;
