//! What a Windows image file (`install.wim`, `install.esd`) holds: its
//! editions, release and architecture, from the XML every WIM carries
//! uncompressed. Only the header and that XML are read.

use std::fmt;

/// The header is this long, and the XML is capped at this.
pub const HEADER_LEN: usize = 208;
const MAX_XML: u64 = 8 << 20;

/// One image (edition) inside a WIM.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Edition {
    pub index: u32,
    /// "Windows 11 Pro"
    pub name: String,
    /// "Professional"
    pub edition_id: String,
    /// "amd64", "arm64", "x86"
    pub arch: Option<String>,
    /// (major, minor, build, update): (10, 0, 26100, 1742)
    pub version: Option<(u32, u32, u32, u32)>,
    pub languages: Vec<String>,
    pub server: bool,
}

impl Edition {
    /// "Windows 11 24H2", "Windows Server 2022", or the build if it isn't known.
    pub fn release(&self) -> Option<String> {
        let (major, _, build, _) = self.version?;
        Some(release_name(major, build, self.server))
    }
}

/// Where a WIM keeps its XML: (offset, length) in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub xml_offset: u64,
    pub xml_len: u64,
    pub part: u16,
    pub parts: u16,
    pub images: u32,
}

/// Reads a WIM header from its first [`HEADER_LEN`] bytes.
pub fn header(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < HEADER_LEN || &bytes[..8] != b"MSWIM\0\0\0" {
        return None;
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    // A resource header: 7-byte stored size, a flags byte, offset, original size.
    let xml_len = u64_at(72) & 0x00FF_FFFF_FFFF_FFFF;
    let xml_offset = u64_at(80);
    (xml_len > 0 && xml_len <= MAX_XML).then_some(Header { xml_offset, xml_len, part: u16_at(40), parts: u16_at(42), images: u32_at(44) })
}

/// The editions described by a WIM's XML (UTF-16LE, as WIMs store it).
pub fn editions(xml: &[u8]) -> Vec<Edition> {
    let units: Vec<u16> = xml.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let text = String::from_utf16_lossy(&units);
    let mut out = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<IMAGE ") {
        let Some(len) = rest[start..].find("</IMAGE>") else { break };
        let image = &rest[start..start + len];
        rest = &rest[start + len + 8..];
        let index = image.split("INDEX=\"").nth(1).and_then(|s| s.split('"').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
        // The image's own NAME, not one inside a nested element.
        let windows = tag(image, "WINDOWS").unwrap_or("");
        let outside = image.replace(windows, "");
        let num = |name: &str| tag(windows, "VERSION").and_then(|v| tag(v, name)).and_then(|n| n.trim().parse().ok());
        let version = match (num("MAJOR"), num("MINOR"), num("BUILD")) {
            (Some(major), Some(minor), Some(build)) => Some((major, minor, build, num("SPBUILD").unwrap_or(0))),
            _ => None,
        };
        let edition_id = tag(windows, "EDITIONID").map(unescape).unwrap_or_default();
        let installation = tag(windows, "INSTALLATIONTYPE").unwrap_or("");
        let languages = tag(windows, "LANGUAGES")
            .map(|l| l.split("<LANGUAGE>").skip(1).filter_map(|s| s.split('<').next()).map(unescape).collect())
            .unwrap_or_default();
        out.push(Edition {
            index,
            name: tag(&outside, "DISPLAYNAME").or_else(|| tag(&outside, "NAME")).map(unescape).unwrap_or_default(),
            arch: tag(windows, "ARCH").and_then(|a| arch_name(a.trim())).map(String::from),
            version,
            languages,
            server: installation.starts_with("Server") || edition_id.starts_with("Server"),
            edition_id,
        });
    }
    out
}

/// The text between `<name>` and `</name>`, the first time it appears.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let len = text[start..].find(&close)?;
    Some(&text[start..start + len])
}

fn unescape(text: &str) -> String {
    text.trim().replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// WIM architecture numbers (Windows' PROCESSOR_ARCHITECTURE values).
fn arch_name(code: &str) -> Option<&'static str> {
    match code {
        "0" => Some("x86"),
        "5" => Some("arm"),
        "9" => Some("amd64"),
        "12" => Some("arm64"),
        _ => None,
    }
}

/// The release a Windows build number belongs to.
pub fn release_name(major: u32, build: u32, server: bool) -> String {
    if major != 10 {
        return format!("Windows {major} (build {build})");
    }
    if server {
        let year = match build {
            14393 => "2016",
            17763 => "2019",
            20348 => "2022",
            26100 => "2025",
            _ => return format!("Windows Server (build {build})"),
        };
        return format!("Windows Server {year}");
    }
    let (product, release) = match build {
        b if b >= 22000 => (
            "Windows 11",
            match b {
                22000 => "21H2",
                22621 => "22H2",
                22631 => "23H2",
                26100 => "24H2",
                26200 => "25H2",
                _ => "",
            },
        ),
        b => (
            "Windows 10",
            match b {
                10240 => "1507",
                10586 => "1511",
                14393 => "1607",
                15063 => "1703",
                16299 => "1709",
                17134 => "1803",
                17763 => "1809",
                18362 => "1903",
                18363 => "1909",
                19041 => "2004",
                19042 => "20H2",
                19043 => "21H1",
                19044 => "21H2",
                19045 => "22H2",
                _ => "",
            },
        ),
    };
    if release.is_empty() { format!("{product} (build {build})") } else { format!("{product} {release}") }
}

/// The editions of a WIM, read for a summary line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Info {
    pub editions: Vec<Edition>,
    /// This file is one part of a split (`.swm`) set.
    pub split: bool,
}

impl Info {
    /// The release of the first edition with a known version.
    pub fn release(&self) -> Option<String> {
        self.editions.iter().find_map(Edition::release)
    }

    pub fn arch(&self) -> Option<String> {
        self.editions.iter().find_map(|e| e.arch.clone())
    }

    /// "10.0.26100.1742"
    pub fn build(&self) -> Option<String> {
        let (a, b, c, d) = self.editions.iter().find_map(|e| e.version)?;
        Some(format!("{a}.{b}.{c}.{d}"))
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if !self.edition_id.is_empty() && !self.name.contains(&self.edition_id) {
            write!(f, " ({})", self.edition_id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A WIM header followed by `xml` (as UTF-16LE with a BOM), as WIMs lay
    /// it out in their last resource.
    pub fn fake_wim(xml: &str) -> Vec<u8> {
        let mut xml_bytes = vec![0xFF, 0xFE];
        xml_bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        let mut wim = vec![0u8; HEADER_LEN];
        wim[..8].copy_from_slice(b"MSWIM\0\0\0");
        wim[8..12].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        wim[40..42].copy_from_slice(&1u16.to_le_bytes());
        wim[42..44].copy_from_slice(&1u16.to_le_bytes());
        wim[44..48].copy_from_slice(&2u32.to_le_bytes());
        wim[72..80].copy_from_slice(&(xml_bytes.len() as u64 | (2 << 56)).to_le_bytes());
        wim[80..88].copy_from_slice(&(HEADER_LEN as u64).to_le_bytes());
        wim[88..96].copy_from_slice(&(xml_bytes.len() as u64).to_le_bytes());
        wim.extend(xml_bytes);
        wim
    }

    pub const WINDOWS_11_XML: &str = r#"<WIM><TOTALBYTES>5000000000</TOTALBYTES>
<IMAGE INDEX="1"><WINDOWS><ARCH>9</ARCH><PRODUCTNAME>Microsoft&#174; Windows&#174; Operating System</PRODUCTNAME><EDITIONID>Core</EDITIONID><INSTALLATIONTYPE>Client</INSTALLATIONTYPE><LANGUAGES><LANGUAGE>en-US</LANGUAGE><DEFAULT>en-US</DEFAULT></LANGUAGES><VERSION><MAJOR>10</MAJOR><MINOR>0</MINOR><BUILD>26100</BUILD><SPBUILD>1742</SPBUILD><SPLEVEL>0</SPLEVEL></VERSION></WINDOWS><NAME>Windows 11 Home</NAME><DESCRIPTION>Windows 11 Home</DESCRIPTION><DISPLAYNAME>Windows 11 Home</DISPLAYNAME></IMAGE>
<IMAGE INDEX="2"><WINDOWS><ARCH>9</ARCH><EDITIONID>Professional</EDITIONID><INSTALLATIONTYPE>Client</INSTALLATIONTYPE><VERSION><MAJOR>10</MAJOR><MINOR>0</MINOR><BUILD>26100</BUILD><SPBUILD>1742</SPBUILD></VERSION></WINDOWS><NAME>Windows 11 Pro</NAME><DISPLAYNAME>Windows 11 Pro</DISPLAYNAME></IMAGE></WIM>"#;

    #[test]
    fn editions_from_the_xml() {
        let wim = fake_wim(WINDOWS_11_XML);
        let h = header(&wim).unwrap();
        assert_eq!((h.images, h.part, h.parts, h.xml_offset), (2, 1, 1, HEADER_LEN as u64));
        let xml = &wim[h.xml_offset as usize..(h.xml_offset + h.xml_len) as usize];
        let found = editions(xml);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Windows 11 Home");
        assert_eq!(found[0].edition_id, "Core");
        assert_eq!(found[0].languages, ["en-US"]);
        assert_eq!(found[1].to_string(), "Windows 11 Pro (Professional)");
        assert_eq!(found[1].arch.as_deref(), Some("amd64"));
        assert_eq!(found[1].release().as_deref(), Some("Windows 11 24H2"));
        let info = Info { editions: found, split: false };
        assert_eq!(info.build().as_deref(), Some("10.0.26100.1742"));
    }

    #[test]
    fn releases_by_build() {
        assert_eq!(release_name(10, 19045, false), "Windows 10 22H2");
        assert_eq!(release_name(10, 22631, false), "Windows 11 23H2");
        assert_eq!(release_name(10, 26200, false), "Windows 11 25H2");
        assert_eq!(release_name(10, 27000, false), "Windows 11 (build 27000)");
        assert_eq!(release_name(10, 20348, true), "Windows Server 2022");
    }

    #[test]
    fn junk_isnt_a_wim() {
        assert_eq!(header(&[0u8; 300]), None);
        assert_eq!(header(b"MSWIM\0\0\0"), None);
        assert!(editions(&[0xFF; 99]).is_empty());
        assert!(editions("<IMAGE INDEX=\"1\"><NAME>x".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>().as_slice()).is_empty());
    }
}
