use super::*;
use std::io::Cursor;
use std::path::Path;
use std::process::Command;

fn item(path: &str, is_dir: bool, size: u64) -> Item {
    Item { path: path.into(), is_dir, size }
}

/// Lays out and writes a volume of `size` bytes with `items`, the files
/// filled with `content(path)`, and returns its bytes.
fn volume(size: u64, sector: u32, items: &[Item]) -> Vec<u8> {
    let layout = layout(size, sector, 1 << 20, "TESTVOL", 0x1234_5678_9ABC_DEF0, items.to_vec(), 1_762_862_400).unwrap();
    let mut cache = Cache::new(Cursor::new(vec![0xF6u8; size as usize]), size, sector as usize).unwrap();
    for (i, it) in items.iter().enumerate().filter(|(_, it)| !it.is_dir) {
        if let Some(at) = layout.data_offset(i) {
            cache.write_at(at, &bytes(&it.path, it.size)).unwrap();
        }
    }
    write_tables(&mut cache, 0, &layout).unwrap();
    cache.into_inner().unwrap().into_inner()
}

fn bytes(path: &str, len: u64) -> Vec<u8> {
    let seed = path.bytes().map(u64::from).sum::<u64>();
    (0..len).map(|i| ((i + seed) * 13 % 256) as u8).collect()
}

fn sample() -> Vec<Item> {
    vec![
        item("bootmgr", false, 400_000),
        item("efi", true, 0),
        item("efi/boot", true, 0),
        item("efi/boot/bootx64.efi", false, 1500),
        item("sources", true, 0),
        item("sources/install.wim", false, 70_000),
        item("sources/A Long Name With Spaces.txt", false, 13),
        item("sources/empty", false, 0),
        item("sources/MixedCase.Dll", false, 5000),
        item("sources/ünïcödé ✓.txt", false, 10),
        item("README.TXT", false, 3),
        item("autounattend.xml", false, 600),
    ]
}

/// A folder of `n` files: big enough for a tree of index blocks.
fn big_folder(n: usize) -> Vec<Item> {
    let mut items = vec![item("sources", true, 0)];
    items.extend((0..n).map(|i| item(&format!("sources/file number {i:05} with a longish name.dll"), false, (i % 7) as u64 * 100)));
    items
}

fn tool(name: &str) -> Option<String> {
    ["", "/usr/sbin/", "/sbin/", "/usr/bin/"].iter().map(|d| format!("{d}{name}")).find(|p| Command::new(p).arg("--help").output().is_ok())
}

fn run(program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
}

/// ntfs-3g's tools open the volume, list it and read every file back.
fn ntfs3g_reads(image: &Path, items: &[Item]) {
    let (Some(fix), Some(cat), Some(ls)) = (tool("ntfsfix"), tool("ntfscat"), tool("ntfsls")) else {
        eprintln!("skipped: no ntfs-3g tools");
        return;
    };
    let img = image.to_str().unwrap();
    let (ok, text) = run(&fix, &["-n", img]);
    assert!(ok, "ntfsfix: {text}");
    let (ok, text) = run(&ls, &["-R", "-a", img]);
    assert!(ok, "ntfsls: {text}");
    for it in items {
        let name = it.path.rsplit('/').next().unwrap();
        assert!(text.contains(name), "ntfsls doesn't list {}:\n{text}", it.path);
    }
    for it in items.iter().filter(|i| !i.is_dir) {
        let out = Command::new(&cat).arg(img).arg(&it.path).output().unwrap();
        assert!(out.status.success(), "ntfscat {}: {}", it.path, String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout == bytes(&it.path, it.size), "{} reads back different", it.path);
    }
}

#[test]
fn ntfs3g_reads_back_every_file() {
    let dir = tempfile::tempdir().unwrap();
    for (size, sector, items) in
        [(64u64 << 20, 512u32, sample()), (64 << 20, 4096, sample()), (64 << 20, 512, big_folder(40)), (128 << 20, 512, big_folder(3000))]
    {
        let path = dir.path().join("vol.img");
        std::fs::write(&path, volume(size, sector, &items)).unwrap();
        ntfs3g_reads(&path, &items);
    }
}

#[test]
fn security_descriptors_hash_as_ntfs_does() {
    // mkntfs's descriptor for system files and the hash it stores for it.
    let system = security_descriptor(&[(sid(5, &[18]), 0x0012_0089, 0), (sid(5, &[32, 544]), 0x0012_0089, 0)]);
    assert_eq!(system.len(), 0x68);
    assert_eq!(security_hash(&system), 0xF803_12F0);
}

#[test]
fn runlists() {
    assert_eq!(runlist(&[(Some(4), 0x700)]), [0x12, 0x00, 0x07, 0x04, 0]);
    assert_eq!(runlist(&[(Some(0), 2)]), [0x11, 0x02, 0x00, 0]);
    assert_eq!(runlist(&[(None, 0xFFFF)]), [0x03, 0xFF, 0xFF, 0x00, 0]);
    assert_eq!(runlist(&[(Some(0x80), 1)]), [0x21, 0x01, 0x80, 0x00, 0]);
}

#[test]
fn what_cant_be_laid_out() {
    let lay = |items: Vec<Item>| layout(64 << 20, 512, 1 << 20, "T", 1, items, 0);
    assert!(lay(vec![item("a/b", false, 1)]).is_err(), "no parent");
    assert!(lay(vec![item("a", false, 1), item("A", false, 1)]).is_err(), "names differing in case");
    assert!(lay(vec![item(&"n".repeat(256), false, 1)]).is_err(), "too long");
    assert!(matches!(lay(vec![item("big", false, 100 << 20)]), Err(Error::TooSmall { .. })));
}

/// Writes sample volumes for checking by hand: NTFS_OUT=dir cargo test -- --ignored write_samples
#[test]
#[ignore]
fn write_samples() {
    let out = std::path::PathBuf::from(std::env::var("NTFS_OUT").unwrap());
    std::fs::write(out.join("sample.img"), volume(64 << 20, 512, &sample())).unwrap();
    std::fs::write(out.join("sample4k.img"), volume(64 << 20, 4096, &sample())).unwrap();
    std::fs::write(out.join("big.img"), volume(128 << 20, 512, &big_folder(3000))).unwrap();
}

#[test]
fn the_upcase_table_is_windows() {
    let table = upcase_table();
    assert_eq!(table.iter().enumerate().filter(|&(c, &u)| c as u16 != u).count(), 973);
    let up = |c: char| char::from_u32(u32::from(table[c as usize])).unwrap();
    assert_eq!((up('a'), up('z'), up('ä'), up('ÿ'), up('ω'), up('я')), ('A', 'Z', 'Ä', 'Ÿ', 'Ω', 'Я'));
    // Left alone by Windows, though Unicode upper-cases them.
    assert_eq!((up('µ'), up('ı'), up('ſ'), up('ς')), ('µ', 'ı', 'ſ', 'ς'));
    // The same as mkntfs writes, when it's installed to ask.
    let Some(mkntfs) = tool("mkntfs") else { return };
    let Some(cat) = tool("ntfscat") else { return };
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("ref.img");
    std::fs::write(&img, vec![0u8; 16 << 20]).unwrap();
    let (ok, text) = run(&mkntfs, &["-q", "-F", "-f", img.to_str().unwrap()]);
    assert!(ok, "{text}");
    let out = Command::new(cat).arg("-f").arg(&img).arg("$UpCase").output().unwrap();
    let theirs: Vec<u16> = out.stdout.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    assert!(theirs == table, "differs from mkntfs's $UpCase");
}

#[test]
fn the_ntfs_crate_reads_back_every_file() {
    for (size, sector, items) in [(64u64 << 20, 512u32, sample()), (64 << 20, 4096, sample()), (128 << 20, 512, big_folder(3000))] {
        let mut image = Cursor::new(volume(size, sector, &items));
        let reader = Reader::new(&mut image).unwrap();
        for it in items.iter().filter(|i| !i.is_dir) {
            let mut back = Vec::new();
            let n = reader
                .read(&mut image, &it.path, &mut |b| {
                    back.extend_from_slice(b);
                    Ok(())
                })
                .unwrap_or_else(|e| panic!("{}: {e}", it.path));
            assert_eq!(n, it.size);
            assert!(back == bytes(&it.path, it.size), "{}", it.path);
        }
        assert!(reader.read(&mut image, "sources/nothing", &mut |_| Ok(())).is_err());
    }
}
