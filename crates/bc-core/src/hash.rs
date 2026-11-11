//! SHA-256 of images and drives.

use std::io::{self, Read};

use sha2::{Digest, Sha256};

pub type Sha256Digest = [u8; 32];

/// How much is read or written at a time.
pub const CHUNK: usize = 4 << 20;

pub fn to_hex(digest: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    digest.iter().flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 0xf)]]).map(char::from).collect()
}

/// A SHA-256 written as 64 hex digits, either case.
pub fn parse_hex(text: &str) -> Option<Sha256Digest> {
    let text = text.trim();
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Hashes exactly `len` bytes from `reader`; ending sooner is an error.
/// `progress` hears the running total.
pub fn hash_exact<R: Read>(reader: &mut R, len: u64, progress: &mut dyn FnMut(u64)) -> io::Result<Sha256Digest> {
    let mut buf = vec![0u8; CHUNK];
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    while done < len {
        let want = (len - done).min(CHUNK as u64) as usize;
        let n = read_some(reader, &mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("ended after {done} of {len} bytes")));
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
    Ok(hasher.finalize().into())
}

/// One `read`, retried when a signal interrupts it.
pub(crate) fn read_some<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hashes_a_known_value() {
        let digest = hash_exact(&mut Cursor::new(b"abc"), 3, &mut |_| {}).unwrap();
        assert_eq!(to_hex(&digest), ABC);
    }

    #[test]
    fn hex_round_trips_and_rejects_junk() {
        assert_eq!(parse_hex(ABC).map(|d| to_hex(&d)).as_deref(), Some(ABC));
        assert_eq!(parse_hex(&ABC.to_uppercase()).map(|d| to_hex(&d)).as_deref(), Some(ABC));
        assert_eq!(parse_hex("abc"), None);
        assert_eq!(parse_hex(&"zz".repeat(32)), None);
        assert_eq!(parse_hex(&"é".repeat(32)), None);
    }

    #[test]
    fn a_short_input_is_an_error() {
        let err = hash_exact(&mut Cursor::new(b"ab"), 3, &mut |_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
