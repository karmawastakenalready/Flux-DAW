//! Crafted hostile archives must be refused without writing anything outside the destination.

use flux_assets::extract::{ArchiveFormat, ExtractOptions, Limits, extract, index_archive};
use std::fs;
use std::io::Write;
use std::path::Path;
use zip::write::SimpleFileOptions;

// ---- tar --------------------------------------------------------------------------------------------------

/// One raw ustar entry (the `tar` crate refuses to build most of these names, so headers are written by hand).
fn tar_entry(name: &[u8], kind: u8, data: &[u8], link: &[u8]) -> Vec<u8> {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[148..156].copy_from_slice(b"        ");
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut out = h.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out
}

fn tar_of(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out: Vec<u8> = entries.concat();
    out.extend_from_slice(&[0u8; 1024]);
    out
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn file(name: &str, data: &[u8]) -> Vec<u8> {
    tar_entry(name.as_bytes(), b'0', data, b"")
}

/// Run an extraction in `<tmp>/out` and check nothing appeared next to it.
fn try_extract(bytes: &[u8], format: ArchiveFormat, opts: &ExtractOptions) -> (anyhow::Result<flux_assets::extract::ExtractReport>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join(format!("a.{}", format.as_str()));
    fs::write(&archive, bytes).unwrap();
    let out = tmp.path().join("out");
    let r = extract(&archive, format, &out, opts);
    let siblings: Vec<String> = fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    for s in &siblings {
        assert!(s == "out" || s.starts_with("a."), "extraction wrote {s} outside the destination");
    }
    (r, tmp)
}

fn assert_refused(bytes: &[u8], format: ArchiveFormat, what: &str, expect: &str) {
    let (r, _tmp) = try_extract(bytes, format, &ExtractOptions::default());
    match r {
        Ok(_) => panic!("{what}: archive was accepted"),
        Err(e) => assert!(format!("{e:#}").contains(expect), "{what}: unexpected error {e:#}"),
    }
}

#[test]
fn tar_good_archive_extracts() {
    let t = tar_of(&[tar_entry(b"top/", b'5', b"", b""), file("top/a.txt", b"hello"), file("top/sub/b.bin", &[7u8; 1500])]);
    for (bytes, fmt) in [(t.clone(), ArchiveFormat::Tar), (gz(&t), ArchiveFormat::TarGz)] {
        let (r, tmp) = try_extract(&bytes, fmt, &ExtractOptions { strip: 1, ..Default::default() });
        let r = r.unwrap();
        assert_eq!((r.files, r.bytes), (2, 1505));
        let out = tmp.path().join("out");
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(out.join("sub/b.bin")).unwrap(), vec![7u8; 1500]);
    }
}

#[test]
fn tar_hostile_entries_are_refused() {
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("parent traversal", tar_of(&[file("../evil.txt", b"x")]), "'..'"),
        ("nested traversal", tar_of(&[file("a/../../evil.txt", b"x")]), "'..'"),
        ("backslash traversal", tar_of(&[file("..\\evil.txt", b"x")]), "'..'"),
        ("absolute path", tar_of(&[file("/tmp/evil.txt", b"x")]), "absolute"),
        ("drive prefix", tar_of(&[file("C:/evil.txt", b"x")]), "drive"),
        ("drive-relative", tar_of(&[file("C:evil.txt", b"x")]), "drive"),
        ("symlink", tar_of(&[tar_entry(b"link", b'2', b"", b"../../etc/passwd")]), "link"),
        ("hard link", tar_of(&[file("a.txt", b"x"), tar_entry(b"b.txt", b'1', b"", b"a.txt")]), "link"),
        ("device", tar_of(&[tar_entry(b"dev", b'3', b"", b"")]), "special"),
        ("fifo", tar_of(&[tar_entry(b"fifo", b'6', b"", b"")]), "special"),
        ("duplicate", tar_of(&[file("x.txt", b"1"), file("x.txt", b"2")]), "collides"),
        ("case collision", tar_of(&[file("Read.me", b"1"), file("READ.ME", b"2")]), "collides"),
        ("file then folder", tar_of(&[file("x", b"1"), file("x/y", b"2")]), "collides"),
        ("reserved name", tar_of(&[file("dir/CON.txt", b"x")]), "reserved"),
        ("alternate stream", tar_of(&[file("a.txt:stream", b"x")]), "not allowed"),
        ("trailing dot", tar_of(&[file("evil.", b"x")]), "dot or space"),
    ];
    for (what, t, expect) in &cases {
        assert_refused(t, ArchiveFormat::Tar, what, expect);
        assert_refused(&gz(t), ArchiveFormat::TarGz, what, expect);
    }
}

#[test]
fn tar_bomb_and_entry_limits() {
    let big = vec![0u8; 3 << 20];
    let t = gz(&tar_of(&[file("zeros.bin", &big)]));
    assert!(t.len() < 64 << 10, "test setup: compresses well");
    let limits = Limits { max_total_bytes: 1 << 20, max_entries: 100, max_entry_bytes: 1 << 20 };
    let (r, _tmp) = try_extract(&t, ArchiveFormat::TarGz, &ExtractOptions { limits, ..Default::default() });
    assert!(format!("{:#}", r.unwrap_err()).contains("limit"));
    let many: Vec<Vec<u8>> = (0..50).map(|i| file(&format!("f{i}"), b"x")).collect();
    let limits = Limits { max_total_bytes: 1 << 20, max_entries: 10, max_entry_bytes: 1 << 20 };
    let (r, _tmp) = try_extract(&tar_of(&many), ArchiveFormat::Tar, &ExtractOptions { limits, ..Default::default() });
    assert!(format!("{:#}", r.unwrap_err()).contains("entries"));
}

#[test]
fn truncated_and_garbage_archives_fail_cleanly() {
    let t = tar_of(&[file("a.txt", &[1u8; 5000])]);
    let (r, _tmp) = try_extract(&t[..2000], ArchiveFormat::Tar, &ExtractOptions::default());
    assert!(r.is_err());
    let (r, _tmp) = try_extract(&gz(&t)[..100], ArchiveFormat::TarGz, &ExtractOptions::default());
    assert!(r.is_err());
    let (r, _tmp) = try_extract(b"this is not an archive at all", ArchiveFormat::Zip, &ExtractOptions::default());
    assert!(r.is_err());
    let (r, _tmp) = try_extract(b"\xFD7zXZ\0garbage", ArchiveFormat::TarXz, &ExtractOptions::default());
    assert!(r.is_err());
}

// ---- zip --------------------------------------------------------------------------------------------------

fn zip_of(f: impl FnOnce(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>)) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    f(&mut w);
    w.finish().unwrap().into_inner()
}

fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
}

fn zip_file(name: &str, data: &[u8]) -> Vec<u8> {
    zip_of(|w| {
        w.start_file(name, stored()).unwrap();
        w.write_all(data).unwrap();
    })
}

#[test]
fn zip_good_archive_extracts_with_include() {
    let z = zip_of(|w| {
        w.add_directory("Surge XT.vst3/", stored()).unwrap();
        w.start_file("Surge XT.vst3/Contents/x64-win/Surge XT.vst3", stored()).unwrap();
        w.write_all(b"dll").unwrap();
        w.start_file("README.txt", SimpleFileOptions::default()).unwrap();
        w.write_all(b"readme").unwrap();
    });
    let opts = ExtractOptions { include: vec!["Surge XT.vst3/**".into()], ..Default::default() };
    let (r, tmp) = try_extract(&z, ArchiveFormat::Zip, &opts);
    let r = r.unwrap();
    assert_eq!((r.files, r.filtered_out), (1, 1));
    assert!(tmp.path().join("out/Surge XT.vst3/Contents/x64-win/Surge XT.vst3").is_file());
    assert!(!tmp.path().join("out/README.txt").exists());
    // index_archive sees the same files without writing them.
    let (idx, _) = index_archive(&tmp.path().join("a.zip"), ArchiveFormat::Zip, &opts, tmp.path()).unwrap();
    assert_eq!(idx.len(), 1);
    assert_eq!(idx.entries()[0].sha256, flux_assets::hash::sha256_bytes(b"dll"));
}

#[test]
fn zip_hostile_entries_are_refused() {
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("parent traversal", zip_file("../evil.txt", b"x"), "'..'"),
        ("backslash traversal", zip_file("sub\\..\\..\\evil.txt", b"x"), "'..'"),
        ("absolute", zip_file("/evil.txt", b"x"), "absolute"),
        ("drive", zip_file("C:\\Windows\\evil.dll", b"x"), "drive"),
        ("unc", zip_file("\\\\server\\share\\evil", b"x"), "absolute"),
        (
            "symlink",
            zip_of(|w| {
                w.add_symlink("link", "../../outside", stored()).unwrap();
            }),
            "link",
        ),
        (
            "case collision",
            zip_of(|w| {
                w.start_file("A.wav", stored()).unwrap();
                w.start_file("a.WAV", stored()).unwrap();
            }),
            "collides",
        ),
        ("reserved", zip_file("aux.wav", b"x"), "reserved"),
    ];
    for (what, z, expect) in &cases {
        assert_refused(z, ArchiveFormat::Zip, what, expect);
    }
}

#[test]
fn zip_bomb_is_stopped_by_actual_bytes() {
    let z = zip_of(|w| {
        w.start_file("zeros.bin", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
        w.write_all(&vec![0u8; 8 << 20]).unwrap();
    });
    assert!(z.len() < 100 << 10);
    let limits = Limits { max_total_bytes: 1 << 20, max_entries: 10, max_entry_bytes: 1 << 20 };
    let (r, _tmp) = try_extract(&z, ArchiveFormat::Zip, &ExtractOptions { limits, ..Default::default() });
    assert!(format!("{:#}", r.unwrap_err()).contains("limit"));
}

#[test]
fn nothing_is_left_behind_on_failure_inside_dest() {
    // Good entries before a bad one: the caller (fetch) extracts into a staging folder it deletes on error, so
    // all that matters here is that nothing escapes; this checks the error comes before any escape.
    let t = tar_of(&[file("ok.txt", b"fine"), file("../escape.txt", b"bad")]);
    let (r, tmp) = try_extract(&t, ArchiveFormat::Tar, &ExtractOptions::default());
    assert!(r.is_err());
    assert!(!tmp.path().join("escape.txt").exists());
    assert!(!Path::new(&tmp.path().join("..").join("escape.txt")).exists());
}
