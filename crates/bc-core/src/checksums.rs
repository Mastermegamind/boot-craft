//! Finding an image's published SHA-256 in the checksum files that usually
//! sit next to it (`SHA256SUMS`, `<image>.sha256`, Fedora's `*-CHECKSUM`...).
//!
//! This proves the download is intact, not that it is genuine: someone who
//! can replace the image can replace a checksum file beside it too.

use std::fs;
use std::path::{Path, PathBuf};

use crate::hash::{Sha256Digest, parse_hex};

/// Checksum files bigger than this aren't read.
const MAX_FILE: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub sha256: Sha256Digest,
    /// The checksum file it came from.
    pub from: PathBuf,
}

/// The SHA-256 published for `image`, if a checksum file next to it lists it.
pub fn find(image: &Path) -> Option<Published> {
    let name = image.file_name()?.to_str()?;
    let dir = image.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    candidates(dir, name).into_iter().find_map(|file| {
        let text = read_small(&file)?;
        lookup(&text, name).map(|sha256| Published { sha256, from: file })
    })
}

/// Checksum files that could list `name`, the most specific first.
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = [".sha256", ".sha256sum", ".sha256.txt"].iter().map(|ext| dir.join(format!("{name}{ext}"))).collect();
    let mut shared: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let file = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
            file.starts_with("sha256sum") || file.ends_with("checksum") || file.ends_with("-checksum.txt") || file == "checksums.txt"
        })
        .collect();
    shared.sort();
    files.extend(shared);
    files
}

fn read_small(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    String::from_utf8(fs::read(path).ok()?).ok()
}

/// The hash for `name` in a checksum file's text, in any of the usual forms:
/// `<hex>  name`, `<hex> *name`, `SHA256 (name) = <hex>`, or a lone `<hex>`
/// in a file made for one image.
fn lookup(text: &str, name: &str) -> Option<Sha256Digest> {
    let mut lone = None;
    let mut lines = 0;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        lines += 1;
        if let Some(rest) = line.strip_prefix("SHA256 (") {
            match rest.split_once(") = ") {
                Some((file, hex)) if same_file(file, name) => return parse_hex(hex),
                _ => continue,
            }
        }
        let (hex, file) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let file = file.trim_start().trim_start_matches('*');
        match parse_hex(hex) {
            Some(digest) if same_file(file, name) => return Some(digest),
            Some(digest) if file.is_empty() => lone = Some(digest),
            _ => {}
        }
    }
    lone.filter(|_| lines == 1)
}

fn same_file(listed: &str, name: &str) -> bool {
    let listed = listed.trim().trim_start_matches("./");
    listed == name || listed.rsplit(['/', '\\']).next() == Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::to_hex;

    const A: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const B: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn found(text: &str, name: &str) -> Option<String> {
        lookup(text, name).map(|d| to_hex(&d))
    }

    #[test]
    fn reads_every_common_format() {
        assert_eq!(found(&format!("{B}  other.iso\n{A}  ubuntu.iso\n"), "ubuntu.iso").as_deref(), Some(A));
        assert_eq!(found(&format!("{A} *ubuntu.iso\n"), "ubuntu.iso").as_deref(), Some(A));
        assert_eq!(found(&format!("{A}  ./images/ubuntu.iso\n"), "ubuntu.iso").as_deref(), Some(A));
        assert_eq!(found(&format!("SHA256 (Fedora.iso) = {A}\n"), "Fedora.iso").as_deref(), Some(A));
        assert_eq!(found(&format!("{A}\n"), "anything.img").as_deref(), Some(A));
    }

    #[test]
    fn fedoras_signed_checksum_file() {
        let text = format!(
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n# Fedora.iso: 2 bytes\nSHA256 (Fedora.iso) = {A}\n\
             -----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n"
        );
        assert_eq!(found(&text, "Fedora.iso").as_deref(), Some(A));
    }

    #[test]
    fn other_files_and_junk_are_not_matches() {
        assert_eq!(found(&format!("{A}  other.iso\n"), "ubuntu.iso"), None);
        assert_eq!(found(&format!("{A}\n{B}\n"), "ubuntu.iso"), None, "two lone hashes say nothing about this file");
        assert_eq!(found("not a checksum file", "ubuntu.iso"), None);
    }

    #[test]
    fn finds_the_file_next_to_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("ubuntu.iso");
        fs::write(&image, b"x").unwrap();
        assert_eq!(find(&image), None);
        fs::write(dir.path().join("SHA256SUMS"), format!("{B}  other.iso\n{A}  ubuntu.iso\n")).unwrap();
        let published = find(&image).unwrap();
        assert_eq!((to_hex(&published.sha256), published.from), (A.to_string(), dir.path().join("SHA256SUMS")));
        // A file for this image alone wins over the shared one.
        fs::write(dir.path().join("ubuntu.iso.sha256"), format!("{B}\n")).unwrap();
        assert_eq!(to_hex(&find(&image).unwrap().sha256), B);
    }
}
