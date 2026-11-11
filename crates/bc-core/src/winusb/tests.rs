use super::*;
use crate::image;
use crate::partitions::{self, PartType, guid};
use crate::tree::tests::fixture;
use std::io::Cursor;
use std::process::Command;

const DISK: u64 = 64 << 20;

/// A fixture ISO, decompressed to a real file (the copy reads it in place).
fn iso_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name.trim_end_matches(".gz"));
    fs::write(&path, fixture(name)).unwrap();
    path
}

fn planned(path: &Path, options: &Options) -> Result<(Source, Plan), Error> {
    let info = image::inspect(path)?;
    let plan = plan(&info.source, &info.detected, options)?;
    Ok((info.source, plan))
}

/// Builds the stick on a `DISK`-byte drive full of old junk.
fn make(source: &Source, plan: &Plan, prepared: &Prepared) -> (Built, Vec<u8>) {
    let dev = Cursor::new(vec![0xE5u8; DISK as usize]);
    let (built, dev) = build(dev, DISK, 512, source, plan, prepared, None, &mut |_, _, _| {}).unwrap();
    (built, dev.into_inner())
}

/// The FAT32 file system on a built stick.
fn on_stick(image: &[u8]) -> (partitions::Table, Vec<(String, Vec<u8>)>, String) {
    let table = partitions::read(&mut Cursor::new(image)).unwrap().unwrap();
    let part = &table.parts[0];
    let mut cursor = Cursor::new(image[part.start as usize..(part.start + part.size) as usize].to_vec());
    let fs = fatfs::FileSystem::new(&mut cursor, fatfs::FsOptions::new()).unwrap();
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
    let label = fs.volume_label();
    let mut files = Vec::new();
    let mut dirs = vec![(String::new(), fs.root_dir())];
    while let Some((prefix, dir)) = dirs.pop() {
        for e in dir.iter().map(Result::unwrap).filter(|e| e.file_name() != "." && e.file_name() != "..") {
            let path = format!("{prefix}{}", e.file_name());
            if e.is_dir() {
                dirs.push((format!("{path}/"), e.to_dir()));
            } else {
                let mut bytes = Vec::new();
                e.to_file().read_to_end(&mut bytes).unwrap();
                files.push((path, bytes));
            }
        }
    }
    files.sort();
    (table.clone(), files, label)
}

#[test]
fn a_windows_iso_becomes_a_fat32_stick_with_the_same_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = iso_file(&dir, "windows.iso.gz");
    let (source, plan) = planned(&path, &Options::default()).unwrap();
    assert_eq!((plan.scheme, plan.label.as_str(), plan.split.is_none()), (Scheme::Gpt, "CCCOMA_X64F", true));
    let (built, image) = make(&source, &plan, &Prepared::default());

    let (table, files, label) = on_stick(&image);
    assert_eq!(table.scheme, Scheme::Gpt);
    assert!(table.parts[0].is(guid::MICROSOFT_DATA));
    assert_eq!(label.trim_end(), "CCCOMA_X64F");

    // Byte for byte what the ISO holds, under the same names.
    let mut tree = Tree::open(File::open(&path).unwrap()).unwrap().unwrap();
    let mut expected: Vec<(String, Vec<u8>)> = tree
        .walk(1000)
        .unwrap()
        .into_iter()
        .filter(|(_, e)| !e.is_dir)
        .map(|(p, e)| {
            let mut bytes = Vec::new();
            tree.reader(&e).read_to_end(&mut bytes).unwrap();
            (p, bytes)
        })
        .collect();
    expected.sort();
    assert!(files == expected, "{:?}", files.iter().map(|f| &f.0).collect::<Vec<_>>());
    assert!(files.iter().any(|(p, b)| p == "sources/setup.dll" && b.len() == 3 << 20));
    assert!(files.iter().any(|(p, _)| p == "efi/boot/bootx64.efi"));
    assert_eq!(built.files.len(), expected.len());

    verify(Cursor::new(image), DISK, 512, &built, None, &mut |_, _, _| {}).unwrap();
}

#[test]
fn mbr_sticks_have_an_active_fat32_partition() {
    let dir = tempfile::tempdir().unwrap();
    let path = iso_file(&dir, "windows.iso.gz");
    let (source, plan) = planned(&path, &Options { scheme: Scheme::Mbr, ..Options::default() }).unwrap();
    let (_, image) = make(&source, &plan, &Prepared::default());
    let (table, files, _) = on_stick(&image);
    assert_eq!((table.scheme, &table.parts[0].kind), (Scheme::Mbr, &PartType::Mbr(0x0C)));
    assert_eq!(image[446], 0x80, "active");
    assert!(files.iter().any(|(p, _)| p == "bootmgr"));
}

#[test]
fn setup_options_add_autounattend_xml() {
    let dir = tempfile::tempdir().unwrap();
    let path = iso_file(&dir, "windows.iso.gz");
    let unattend = unattend::Options { bypass_requirements: true, local_account: Some("ada".into()), ..Default::default() };
    let (source, plan) = planned(&path, &Options { unattend: unattend.clone(), ..Options::default() }).unwrap();
    let (built, image) = make(&source, &plan, &Prepared::default());
    let (_, files, _) = on_stick(&image);
    let xml = files.iter().find(|(p, _)| p == "autounattend.xml").map(|(_, b)| String::from_utf8(b.clone()).unwrap()).unwrap();
    assert_eq!(Some(xml), unattend::xml(&unattend, Some("amd64")));
    assert!(built.files.iter().any(|(p, _, _)| p == "autounattend.xml"));

    let bad = unattend::Options { local_account: Some("a/b".into()), ..Default::default() };
    assert!(matches!(planned(&path, &Options { unattend: bad, ..Options::default() }), Err(Error::UnsupportedImage(_))));
    let winpe = iso_file(&dir, "winpe.iso.gz");
    assert!(planned(&winpe, &Options { unattend, ..Options::default() }).is_err(), "no setup options for Windows PE");
    assert!(planned(&winpe, &Options::default()).is_ok());
}

#[test]
fn a_damaged_file_fails_verification() {
    let dir = tempfile::tempdir().unwrap();
    let path = iso_file(&dir, "windows.iso.gz");
    let (source, plan) = planned(&path, &Options::default()).unwrap();
    let (built, mut image) = make(&source, &plan, &Prepared::default());
    let at = image.windows(11).position(|w| w == b"[BUILDINFO]").expect("idwbinfo.txt's bytes on the stick");
    image[at + 3] ^= 1;
    match verify(Cursor::new(image), DISK, 512, &built, None, &mut |_, _, _| {}) {
        Err(Error::FileMismatch { path, .. }) => assert_eq!(path, "sources/idwbinfo.txt"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn refusals_come_before_anything_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let path = iso_file(&dir, "windows.iso.gz");
    // A file other than install.wim over the limit: needs NTFS.
    let tiny = Options { split_over: 100, ..Options::default() };
    let err = planned(&path, &tiny).unwrap_err().to_string();
    assert!(err.contains("NTFS"), "{err}");
    // Not Windows.
    assert!(planned(&iso_file(&dir, "ubuntu.iso.gz"), &Options::default()).is_err());
    // Too small a drive.
    let (source, plan) = planned(&path, &Options::default()).unwrap();
    assert!(matches!(plan.fits(8 << 20), Err(Error::TooSmall { .. })));
    let r = build(Cursor::new(vec![0u8; 8 << 20]), 8 << 20, 512, &source, &plan, &Prepared::default(), None, &mut |_, _, _| {});
    assert!(matches!(r, Err(Error::TooSmall { .. })));
    // Stopping.
    let stop = AtomicBool::new(true);
    let r = build(Cursor::new(vec![0u8; DISK as usize]), DISK, 512, &source, &plan, &Prepared::default(), Some(&stop), &mut |_, _, _| {});
    assert!(matches!(r, Err(Error::Cancelled)));
}

#[test]
fn a_big_install_wim_is_split_into_swm_parts() {
    let have = |p: &str| Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success());
    if !have("wimlib-imagex") || !have("genisoimage") {
        eprintln!("skipped: needs wimlib-imagex and genisoimage");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A real WIM of a few MiB that doesn't compress, inside a Windows-like ISO.
    let content = dir.path().join("content");
    fs::create_dir_all(&content).unwrap();
    let noise: Vec<u8> =
        (0u64..5 << 20).map(|i| (i.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) >> 56) as u8).collect();
    fs::write(content.join("noise.bin"), &noise).unwrap();
    let tree = dir.path().join("iso");
    fs::create_dir_all(tree.join("sources")).unwrap();
    fs::create_dir_all(tree.join("efi/boot")).unwrap();
    fs::write(tree.join("efi/boot/bootx64.efi"), b"MZ").unwrap();
    fs::write(tree.join("sources/boot.wim"), b"MSWIM\0\0\0").unwrap();
    let wim = tree.join("sources/install.wim");
    let ok = Command::new("wimlib-imagex").arg("capture").arg(&content).arg(&wim).args(["Test", "--compress=none"]).output().unwrap();
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    let iso = dir.path().join("win.iso");
    let ok = Command::new("genisoimage")
        .args(["-quiet", "-udf", "-J", "-iso-level", "3", "-V", "TESTWIN", "-o"])
        .arg(&iso)
        .arg(&tree)
        .output()
        .unwrap();
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));

    let options = Options { split_over: 2 << 20, split_part_mib: 2, temp_dir: Some(dir.path().to_path_buf()), ..Options::default() };
    let (source, plan) = planned(&iso, &options).unwrap();
    assert_eq!(plan.split.as_ref().map(|s| s.path.as_str()), Some("sources/install.wim"));
    let mut steps = Vec::new();
    let prepared = prepare(&source, &plan, None, &mut |step, _, _| steps.push(step)).unwrap();
    assert!(steps.contains(&Step::Extracting) && steps.contains(&Step::Splitting));
    assert!(prepared.parts.len() >= 2, "{:?}", prepared.parts);
    let (built, image) = make(&source, &plan, &prepared);
    let (_, files, _) = on_stick(&image);
    let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(names.contains(&"sources/install.swm") && names.contains(&"sources/install2.swm"), "{names:?}");
    assert!(!names.contains(&"sources/install.wim"));
    verify(Cursor::new(image.clone()), DISK, 512, &built, None, &mut |_, _, _| {}).unwrap();

    // The parts are a valid split WIM holding the original file.
    let out = dir.path().join("out");
    for (name, bytes) in files.iter().filter(|(p, _)| p.ends_with(".swm")) {
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join(name.trim_start_matches("sources/")), bytes).unwrap();
    }
    let applied = dir.path().join("applied");
    let ok = Command::new("wimlib-imagex")
        .arg("apply")
        .arg(out.join("install.swm"))
        .arg("1")
        .arg(&applied)
        .arg(format!("--ref={}", out.join("install*.swm").display()))
        .output()
        .unwrap();
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    assert!(fs::read(applied.join("noise.bin")).unwrap() == noise);
    let temp = prepared._dir.as_ref().unwrap().0.clone();
    drop(prepared);
    assert!(!temp.exists(), "the temporary folder is removed");
}

#[test]
fn labels() {
    assert_eq!(fat_label("CCCOMA_X64FRE_EN-US_DV9"), "CCCOMA_X64F");
    assert_eq!(fat_label("Win 11 été"), "WIN 11 _T_");
    assert_eq!(fat_label("  "), "WINDOWS");
}
