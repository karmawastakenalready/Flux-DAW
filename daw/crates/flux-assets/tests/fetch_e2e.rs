//! End to end: fetch from a local HTTP server and a local git repository, verify, drift, repair, re-lock,
//! and the guarantees around re-downloading, size limits and confirmation.

mod common;

use flux_assets::fetch::{self, FetchOptions, Plan};
use flux_assets::manifest;
use flux_assets::verify::{self, IssueKind, State, VerifyOptions};
use flux_assets::workspace::{RecordingUi, Workspace};
use common::{Server, bytes, sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "core.autocrlf=false", "-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(p: &Path, b: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
}

/// A repository whose pinned commit is not the branch tip (so the fetch-by-id path runs).
fn make_repo(dir: &Path) -> (String, String) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "uploadpack.allowFilter", "true"]);
    git(dir, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    write(&dir.join("LICENSE"), b"Creative Commons Legal Code\n\nAttribution 3.0 Unported\n");
    write(&dir.join("Samples/a.wav"), &bytes(70_000, 21));
    write(&dir.join("Samples/notes.txt"), b"line one\nline two\n");
    write(&dir.join("Programs/p.sfz"), b"<region> sample=../Samples/a.wav\n");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "one"]);
    let rev = git(dir, &["rev-parse", "HEAD"]);
    write(&dir.join("Samples/notes.txt"), b"changed later\n");
    git(dir, &["commit", "-q", "-am", "two"]);
    let url = format!("file:///{}", dir.to_string_lossy().replace('\\', "/").trim_start_matches('/'));
    (url, rev)
}

fn zip_kit() -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default();
    for (name, data) in [
        ("Kit-1.0/LICENSE.txt", b"Creative Commons Legal Code\n\nCC0 1.0 Universal\n".to_vec()),
        ("Kit-1.0/kick.wav", bytes(40_000, 22)),
        ("Kit-1.0/snare.wav", bytes(30_000, 23)),
        ("Kit-1.0/kit.sfz", b"<region> sample=kick.wav\n".to_vec()),
    ] {
        w.start_file(name, o).unwrap();
        w.write_all(&data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    srv: Server,
    text: String,
}

const SFZ: &[u8] = b"<control> default_path=../Samples/\n<region> sample=a.wav\n";

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let (git_url, rev) = make_repo(&tmp.path().join("upstream"));
    let srv = Server::start();
    let kit = zip_kit();
    srv.file("/kit.zip", &kit);
    srv.file("/piano-sfz/Piano%20Map.sfz", SFZ);
    let text = format!(
        r#"
schema = 1
[policy]
allowed_hosts = []
download_dir = "dl"
usage_roots = ["projects/*.json"]

[[asset]]
id = "kit"
name = "Test Kit"
kind = "sample-library"
scope = "audio"
path = "instruments/kit"
version = "1.0"
license = {{ id = "CC0-1.0", file = "LICENSE.txt" }}
credit = {{ role = "Drums", text = "Test Kit, CC0" }}
[[asset.source]]
type = "archive"
url = "{base}/kit.zip"
sha256 = "{kit_sha}"
size = {kit_len}
format = "zip"
strip = 1

[[asset]]
id = "piano"
name = "Test Piano"
kind = "sample-library"
scope = "audio"
path = "instruments/piano"
version = "3"
license = {{ id = "CC-BY-3.0", file = "LICENSE" }}
credit = {{ role = "Piano samples", text = "Test Piano by Someone, licensed under CC BY 3.0" }}
[[asset.source]]
type = "git"
url = "{git_url}"
rev = "{rev}"
sparse = ["/LICENSE", "/Samples/"]
download_size = "100k"
[[asset.source]]
type = "files"
base_url = "{base}/piano-sfz"
dest = "sfz"
[[asset.source.file]]
path = "Piano Map.sfz"
sha256 = "{sfz_sha}"
size = {sfz_len}
"#,
        base = srv.base,
        kit_sha = sha256(&kit),
        kit_len = kit.len(),
        sfz_sha = sha256(SFZ),
        sfz_len = SFZ.len(),
    );
    Fixture { _tmp: tmp, root, srv, text }
}

fn open(f: &Fixture) -> Workspace {
    let loaded = manifest::parse_with(&f.text, true).unwrap();
    let mut ws = Workspace::from_loaded(&f.root, loaded, f.root.join("assets.toml"), f.root.join("assets.lock")).unwrap();
    ws.http.retry_delay = std::time::Duration::from_millis(5);
    ws
}

fn all() -> FetchOptions {
    FetchOptions { all: true, yes: true, ..Default::default() }
}

fn check(ws: &Workspace, id: &str) -> verify::AssetCheck {
    let a = ws.loaded.asset(id).unwrap().clone();
    verify::verify_asset(ws, &a, VerifyOptions::default(), &mut RecordingUi::default())
}

fn ids(ws: &Workspace) -> Vec<String> {
    ws.loaded.assets().iter().map(|a| a.id.clone()).collect()
}

#[test]
fn fetch_verify_drift_repair_relock() {
    let f = fixture();
    let mut ws = open(&f);
    let mut ui = RecordingUi::default();

    // Dry run changes nothing.
    let out = fetch::fetch(&mut ws, &[], &FetchOptions { dry_run: true, ..all() }, &mut ui).unwrap();
    assert!(out.iter().all(|o| o.result == "planned"));
    assert!(matches!(out[0].plan, Plan::Install { replace: false, .. }));
    assert!(!f.root.join("instruments").exists() && f.srv.hits().is_empty());

    // Fresh install of both.
    let out = fetch::fetch(&mut ws, &[], &all(), &mut ui).unwrap();
    assert_eq!(out.iter().map(|o| o.result.as_str()).collect::<Vec<_>>(), ["installed", "installed"], "{:?}", ui.lines);
    let kit = f.root.join("instruments/kit");
    let piano = f.root.join("instruments/piano");
    assert_eq!(fs::read(kit.join("kick.wav")).unwrap(), bytes(40_000, 22));
    assert!(piano.join("Samples/a.wav").is_file() && piano.join("sfz/Piano Map.sfz").is_file());
    assert!(!piano.join("Programs").exists(), "sparse checkout keeps Programs out");
    assert_eq!(fs::read(piano.join("Samples/notes.txt")).unwrap(), b"line one\nline two\n", "pinned commit, LF kept");
    assert!(f.root.join("dl/kit/kit.zip").is_file(), "archive cached");
    assert_eq!(ws.lock.assets.len(), 2);
    assert!(f.root.join("assets.lock.d/kit.tsv").is_file());
    assert!(fetch::leftovers(&f.root.join("instruments")).is_empty());
    for id in ["kit", "piano"] {
        let c = check(&ws, id);
        assert_eq!(c.state, State::Ok, "{id}: {:?}", c.issues);
    }

    // Running fetch again downloads nothing.
    f.srv.clear_hits();
    let out = fetch::fetch(&mut ws, &[], &all(), &mut ui).unwrap();
    assert!(out.iter().all(|o| o.result == "present" && o.downloaded == 0));
    assert!(f.srv.hits().is_empty());

    // Drift in the archive install: modified, missing, extra.
    fs::write(kit.join("kick.wav"), b"edited").unwrap();
    fs::remove_file(kit.join("snare.wav")).unwrap();
    fs::write(kit.join("mine.txt"), b"user file").unwrap();
    let c = check(&ws, "kit");
    assert_eq!(c.state, State::Drift);
    assert_eq!((c.count(IssueKind::Modified), c.count(IssueKind::Missing), c.count(IssueKind::Extra)), (1, 1, 1), "{:?}", c.issues);
    // Archive contents are not repaired file by file.
    let e = fetch::repair(&mut ws, "kit", true, &mut ui).unwrap_err();
    assert!(format!("{e:#}").contains("fetch --force"), "{e:#}");
    // --force reinstalls from the cached archive (no download) and swaps folders.
    f.srv.clear_hits();
    let out = fetch::fetch(&mut ws, &["kit".into()], &FetchOptions { force: true, yes: true, ..Default::default() }, &mut ui).unwrap();
    assert_eq!(out[0].result, "replaced", "{:?}", out[0].message);
    assert!(f.srv.hits().is_empty(), "the cached archive was reused");
    assert!(!kit.join("mine.txt").exists());
    assert_eq!(check(&ws, "kit").state, State::Ok);
    assert!(fetch::leftovers(&f.root.join("instruments")).is_empty());

    // Drift in the git install: CRLF rewrite, edit, deleted pinned file.
    fs::write(piano.join("Samples/notes.txt"), b"line one\r\nline two\r\n").unwrap();
    fs::write(piano.join("Samples/a.wav"), b"broken").unwrap();
    fs::remove_file(piano.join("sfz/Piano Map.sfz")).unwrap();
    let c = check(&ws, "piano");
    assert_eq!(c.state, State::Drift);
    assert_eq!((c.count(IssueKind::EolOnly), c.count(IssueKind::Modified), c.count(IssueKind::Missing)), (1, 1, 1), "{:?}", c.issues);
    // Locking refuses a modified install.
    let out = fetch::lock_existing(&mut ws, &["piano".into()], &mut ui).unwrap();
    assert_eq!(out[0].result, "refused", "{}", out[0].message);
    // Repair restores all three.
    f.srv.clear_hits();
    let r = fetch::repair(&mut ws, "piano", true, &mut ui).unwrap();
    assert_eq!(r.restored.len(), 3, "{:?}", r.restored);
    assert_eq!(r.state_after, State::Ok);
    assert_eq!(f.srv.hits_for("/piano-sfz/Piano%20Map.sfz").len(), 1);
    assert_eq!(fs::read(piano.join("Samples/notes.txt")).unwrap(), b"line one\nline two\n");

    // Re-lock from scratch (as for installs made by setup_assets.py): both are adopted.
    fs::remove_file(f.root.join("assets.lock")).unwrap();
    fs::remove_dir_all(f.root.join("assets.lock.d")).unwrap();
    let mut ws = open(&f);
    let all_ids = ids(&ws);
    let out = fetch::lock_existing(&mut ws, &all_ids, &mut ui).unwrap();
    assert!(out.iter().all(|o| o.result == "locked"), "{out:?}");
    for id in ["kit", "piano"] {
        assert_eq!(check(&ws, id).state, State::Ok);
    }
    // ... but not with a file no source accounts for.
    fs::write(kit.join("stray.wav"), b"?").unwrap();
    let out = fetch::lock_existing(&mut ws, &["kit".into()], &mut ui).unwrap();
    assert_eq!(out[0].result, "refused");
    assert!(out[0].message.contains("not accounted for"), "{}", out[0].message);
}

#[test]
fn size_limit_confirmation_and_busy_marker() {
    let f = fixture();
    let mut ws = open(&f);
    // Above the limit: refused, nothing downloaded.
    let mut ui = RecordingUi::default();
    let out = fetch::fetch(&mut ws, &["kit".into()], &FetchOptions { max_size: Some(1000), yes: true, ..Default::default() }, &mut ui).unwrap();
    assert_eq!(out[0].result, "refused");
    assert!(out[0].message.as_deref().unwrap().contains("--max-size"));
    assert!(f.srv.hits().is_empty());
    // Declined confirmation: nothing happens.
    let mut no = RecordingUi { answer: false, ..Default::default() };
    let e = fetch::fetch(&mut ws, &["kit".into()], &FetchOptions::default(), &mut no).unwrap_err();
    assert!(format!("{e:#}").contains("cancelled"));
    assert_eq!(no.questions.len(), 1);
    assert!(f.srv.hits().is_empty());
    // Another operation running.
    fs::write(f.root.join("assets.lock.busy"), "pid 1").unwrap();
    let e = fetch::fetch(&mut ws, &["kit".into()], &FetchOptions { yes: true, ..Default::default() }, &mut ui).unwrap_err();
    assert!(format!("{e:#}").contains("another asset operation"), "{e:#}");
    fs::remove_file(f.root.join("assets.lock.busy")).unwrap();
    // No ids and no --all.
    assert!(fetch::fetch(&mut ws, &[], &FetchOptions::default(), &mut ui).is_err());
}

#[test]
fn a_tampered_archive_is_not_installed() {
    let f = fixture();
    let mut ws = open(&f);
    let mut evil = zip_kit();
    let n = evil.len();
    evil[n / 2] ^= 0xFF;
    f.srv.file("/kit.zip", &evil);
    let out = fetch::fetch(&mut ws, &["kit".into()], &FetchOptions { yes: true, ..Default::default() }, &mut RecordingUi::default()).unwrap();
    assert_eq!(out[0].result, "failed");
    assert!(out[0].message.as_deref().unwrap().contains("checksum mismatch"));
    assert!(!f.root.join("instruments/kit").exists());
    assert!(!f.root.join("dl/kit/kit.zip").exists());
    assert!(fetch::leftovers(&f.root.join("instruments")).is_empty());
}

#[test]
fn cli_status_verify_and_licenses() {
    let f = fixture();
    let mut ws = open(&f);
    write(&f.root.join("projects/song.json"), br#"{"tracks":[{"sfz":"instruments/kit/kit.sfz"}]}"#);
    let mut ui = RecordingUi::default();
    let run = |ws: &mut Workspace, ui: &mut RecordingUi, args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        flux_assets::cli::run_assets_in(ws, &args, ui).unwrap()
    };
    assert_eq!(run(&mut ws, &mut ui, &["fetch", "--all", "--yes"]), 0, "{:?}", ui.lines);
    ui.lines.clear();
    assert_eq!(run(&mut ws, &mut ui, &["status"]), 0);
    let text = ui.lines.join("\n");
    assert!(text.contains("kit ") && text.contains("locked") && text.contains("projects/song.json"), "{text}");
    assert!(text.contains("ok, credit"), "CC BY needs a credit: {text}");
    ui.lines.clear();
    assert_eq!(run(&mut ws, &mut ui, &["verify"]), 0, "{:?}", ui.lines);
    assert!(ui.lines.last().unwrap().starts_with("Summary: 2 ok"), "{:?}", ui.lines);
    fs::write(f.root.join("instruments/kit/kick.wav"), b"x").unwrap();
    ui.lines.clear();
    assert_eq!(run(&mut ws, &mut ui, &["verify", "kit", "--quick"]), 1);
    ui.lines.clear();
    let code = run(&mut ws, &mut ui, &["licenses", "--out", "LICENSES.draft.md", "--credits", "credits.txt"]);
    assert_eq!(code, 0, "{:?}", ui.lines);
    let md = fs::read_to_string(f.root.join("LICENSES.draft.md")).unwrap();
    assert!(md.contains("| `projects/song.json` | Test Kit |"), "{md}");
    assert!(md.contains("| Credit required? | Used by | Source (official) |"), "{md}");
    // without the scan, nothing in the report depends on the local projects
    ui.lines.clear();
    assert_eq!(run(&mut ws, &mut ui, &["licenses", "--no-usage", "--out", "LICENSES.repo.md"]), 0, "{:?}", ui.lines);
    let repo = fs::read_to_string(f.root.join("LICENSES.repo.md")).unwrap();
    assert!(!repo.contains("song.json") && !repo.contains("What each project uses") && !repo.contains("Used by"), "{repo}");
    let rows: Vec<&str> = repo.lines().filter(|l| l.starts_with("| **Test ")).collect();
    assert_eq!(rows.len(), 2, "{repo}");
    for row in rows {
        assert_eq!(row.matches(" | ").count(), 4, "five cells: {row}");
    }
    let credits = fs::read_to_string(f.root.join("credits.txt")).unwrap();
    // The piano is not used by any project, but its credit is required and it is installed, so it stays.
    assert_eq!(credits, "Piano samples: Test Piano by Someone, licensed under CC BY 3.0\nDrums: Test Kit, CC0\n");
    ui.lines.clear();
    fs::write(f.root.join("OLD.md"), "Only the Test Kit is listed here").unwrap();
    assert_eq!(run(&mut ws, &mut ui, &["licenses", "--check", "OLD.md"]), 1);
    assert!(ui.lines.iter().any(|l| l.contains("does not mention 1") && l.contains("piano")), "{:?}", ui.lines);
    assert!(flux_assets::cli::run_assets_in(&mut ws, &["frobnicate".into()], &mut ui).is_err());
}

#[test]
fn check_urls_reports_web_pages_and_optional_assets() {
    let mut f = fixture();
    f.srv.route(
        "/gated.zip",
        common::Route::File { body: b"<html>confirm download</html>".to_vec(), etag: None, ranges: true, content_type: "text/html; charset=utf-8" },
    );
    f.text.push_str(&format!(
        r#"
[[asset]]
id = "gated"
name = "Gated Library"
kind = "sample-library"
scope = "audio"
path = "instruments/gated"
version = "1"
optional = true
license = {{ id = "CC0-1.0", file = "LICENSE" }}
[[asset.source]]
type = "archive"
url = "{base}/gated.zip"
page = "{base}/gated.html"
sha256 = "{sha}"
size = 5000
format = "zip"
"#,
        base = f.srv.base,
        sha = "0".repeat(64),
    ));
    let mut ws = open(&f);
    let probe = FetchOptions { dry_run: true, check_urls: true, ..all() };

    // --all leaves the optional, uninstalled asset out and says so.
    let mut ui = RecordingUi::default();
    let out = fetch::fetch(&mut ws, &[], &probe, &mut ui).unwrap();
    assert_eq!(out.iter().map(|o| (o.id.as_str(), o.result.as_str())).collect::<Vec<_>>(), [("kit", "checked"), ("piano", "checked")]);
    assert!(ui.lines.iter().any(|l| l.contains("not part of --all") && l.ends_with(": gated")), "{:?}", ui.lines);
    assert!(out[1].message.as_deref().unwrap().contains("1 file(s) reachable with the expected sizes"), "{:?}", out[1].message);

    // A web page instead of the archive is a failure with instructions, in plain words.
    let out = fetch::fetch(&mut ws, &["gated".into()], &probe, &mut ui).unwrap();
    let msg = out[0].message.clone().unwrap();
    assert_eq!(out[0].result, "failed");
    assert!(msg.contains("sent a web page (29 bytes)") && msg.contains("download it by hand from") && msg.contains("gated.html"), "{msg}");
    assert!(!msg.contains("Some("), "{msg}");

    // With a cached copy of the pinned size, fetch would install from the cache: not a failure.
    let gated = ws.loaded.asset("gated").unwrap().clone();
    let manifest::Source::Archive(src) = &gated.sources[0] else { panic!("archive source") };
    let cache = ws.archive_cache(&gated, src);
    write(&cache, &vec![0u8; 5000]);
    let out = fetch::fetch(&mut ws, &["gated".into()], &probe, &mut ui).unwrap();
    assert_eq!(out[0].result, "checked", "{:?}", out[0].message);
    assert!(out[0].message.as_deref().unwrap().contains("cached copy"));

    // Once installed, an optional asset is part of --all (to keep it complete).
    write(&f.root.join("instruments/gated/LICENSE"), b"CC0 1.0 Universal\n");
    let out = fetch::fetch(&mut ws, &[], &FetchOptions { dry_run: true, ..all() }, &mut ui).unwrap();
    assert!(out.iter().any(|o| o.id == "gated" && o.plan == Plan::Present), "{out:?}");
    assert!(f.srv.hits_for("/kit.zip").iter().all(|h| h.headers.get("range").map(String::as_str) == Some("bytes=0-0")), "probes only");
}
