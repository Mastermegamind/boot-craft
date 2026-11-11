use std::io;

use crate::drive::Refusal;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("no drive named {0}")]
    NotFound(String),
    #[error("{drive} can't be written: {reason}")]
    Refused { drive: String, reason: Refusal },
    #[error("{0} changed since it was listed (unplugged or swapped?); list the drives again")]
    DriveChanged(String),
    #[error("{0} is in use by the system; unmount it and try again")]
    Busy(String),
    #[error("writing a drive needs administrator rights (on Linux and macOS, run it with sudo)")]
    NeedsAdmin,
    #[error("the image is {image} bytes, larger than the drive ({drive} bytes)")]
    TooSmall { image: u64, drive: u64 },
    #[error("the image is larger than the drive ({drive} bytes); the drive is only partly written and won't boot")]
    TooBigWhileWriting { drive: u64 },
    #[error("{0}")]
    UnsupportedImage(String),
    #[error("stopped")]
    Cancelled,
    #[error("the image is empty")]
    EmptyImage,
    #[error("the image got shorter while it was being written")]
    ImageChanged,
    #[error("verification failed: the drive reads back as SHA-256 {actual}, the image is {expected}")]
    VerifyMismatch { expected: String, actual: String },
}
