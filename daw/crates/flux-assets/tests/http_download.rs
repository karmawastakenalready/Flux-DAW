//! Downloads against a local HTTP server: checksums, resume, retries, redirects, refusals.

mod common;

use flux_assets::http::{Expect, HttpClient, HttpConfig, part_path};
use common::{Route, Server, bytes, sha256};
use std::fs;
use std::time::Duration;

fn client() -> HttpClient {
    let mut cfg = HttpConfig::new(vec!["github.com".into()]);
    cfg.allow_loopback_http = true;
    cfg.retry_delay = Duration::from_millis(5);
    cfg.max_failed_attempts = 3;
    HttpClient::new(cfg).unwrap()
}

fn no_progress() -> impl FnMut(u64, Option<u64>) {
    |_, _| {}
}

#[test]
fn downloads_and_checks() {
    let srv = Server::start();
    let body = bytes(300_000, 1);
    srv.file("/a.bin", &body);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a.bin");
    let d = client().download(&srv.url("/a.bin"), &dest, &Expect::pinned(&sha256(&body), body.len() as u64), &mut no_progress()).unwrap();
    assert_eq!((d.bytes, d.resumed_from, d.requests), (300_000, 0, 1));
    assert_eq!(fs::read(&dest).unwrap(), body);
    assert!(!part_path(&dest).exists());
    // Probe: one byte, size from Content-Range.
    let p = client().probe(&srv.url("/a.bin")).unwrap();
    assert_eq!((p.total_size, p.accepts_ranges, p.status), (Some(300_000), true, 206));
}

/// Leave `<dest>.part` and its metadata as an interrupted run would.
fn interrupted(dest: &std::path::Path, url: &str, sha: &str, have: &[u8], etag: Option<&str>) {
    fs::write(part_path(dest), have).unwrap();
    let meta = serde_json::json!({ "url": url, "expected_sha256": sha, "etag": etag, "last_modified": null, "total": null });
    let mut p = dest.as_os_str().to_owned();
    p.push(".part.json");
    fs::write(p, meta.to_string()).unwrap();
}

#[test]
fn resumes_a_partial_download() {
    let srv = Server::start();
    let body = bytes(200_000, 2);
    srv.file("/r.bin", &body);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("r.bin");
    let url = srv.url("/r.bin");
    interrupted(&dest, &url, &sha256(&body), &body[..50_000], Some("\"200000\""));
    let d = client().download(&url, &dest, &Expect::pinned(&sha256(&body), 200_000), &mut no_progress()).unwrap();
    assert_eq!(d.resumed_from, 50_000);
    assert_eq!(fs::read(&dest).unwrap(), body);
    let hits = srv.hits_for("/r.bin");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].headers.get("range").map(String::as_str), Some("bytes=50000-"));
    assert_eq!(hits[0].headers.get("if-range").map(String::as_str), Some("\"200000\""));
}

#[test]
fn a_part_file_for_another_url_or_checksum_is_discarded() {
    let srv = Server::start();
    let body = bytes(10_000, 12);
    srv.file("/n.bin", &body);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("n.bin");
    interrupted(&dest, "http://127.0.0.1:1/elsewhere", &sha256(&body), &body[..5000], None);
    let d = client().download(&srv.url("/n.bin"), &dest, &Expect::pinned(&sha256(&body), 10_000), &mut no_progress()).unwrap();
    assert_eq!(d.resumed_from, 0);
    assert_eq!(srv.hits_for("/n.bin")[0].headers.get("range"), None);
}

#[test]
fn retries_dropped_connections() {
    let srv = Server::start();
    let body = bytes(120_000, 3);
    srv.route("/d.bin", Route::Drop { body: body.clone(), after: 40_000, times: 2 });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("d.bin");
    let d = client().download(&srv.url("/d.bin"), &dest, &Expect::pinned(&sha256(&body), 120_000), &mut no_progress()).unwrap();
    assert_eq!(d.requests, 3);
    assert_eq!(fs::read(&dest).unwrap(), body);
    let ranges: Vec<Option<String>> = srv.hits_for("/d.bin").iter().map(|h| h.headers.get("range").cloned()).collect();
    assert_eq!(ranges, vec![None, Some("bytes=40000-".into()), Some("bytes=80000-".into())]);
}

#[test]
fn gives_up_without_progress_but_keeps_the_part_file() {
    let srv = Server::start();
    let body = bytes(50_000, 13);
    srv.route("/stuck.bin", Route::Drop { body: body.clone(), after: 0, times: 1000 });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("stuck.bin");
    interrupted(&dest, &srv.url("/stuck.bin"), &sha256(&body), &body[..1000], None);
    let e = client().download(&srv.url("/stuck.bin"), &dest, &Expect::pinned(&sha256(&body), 50_000), &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("run the command again to resume"), "{e:#}");
    assert_eq!(srv.hits_for("/stuck.bin").len(), 3);
    assert_eq!(fs::metadata(part_path(&dest)).unwrap().len(), 1000);
}

#[test]
fn restarts_when_the_server_ignores_ranges_or_the_file_changed() {
    let srv = Server::start();
    let old = bytes(100_000, 4);
    let new = bytes(100_000, 5);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("c.bin");
    let url = srv.url("/c.bin");
    let exp = Expect::pinned(&sha256(&new), 100_000);
    // A server without range support: the 200 answer restarts the file from zero.
    interrupted(&dest, &url, &sha256(&new), &old[..30_000], None);
    srv.route("/c.bin", Route::File { body: new.clone(), etag: None, ranges: false, content_type: "application/zip" });
    let d = client().download(&url, &dest, &exp, &mut no_progress()).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), new);
    assert_eq!(d.bytes, 100_000);
    // A changed ETag: If-Range makes the server send the whole new file.
    fs::remove_file(&dest).unwrap();
    interrupted(&dest, &url, &sha256(&new), &old[..10_000], Some("\"old\""));
    srv.file("/c.bin", &new);
    srv.clear_hits();
    client().download(&url, &dest, &exp, &mut no_progress()).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), new);
    assert_eq!(srv.hits_for("/c.bin")[0].headers.get("if-range").map(String::as_str), Some("\"old\""));
}

#[test]
fn refuses_bad_answers() {
    let srv = Server::start();
    let body = bytes(1000, 6);
    let dir = tempfile::tempdir().unwrap();
    let c = client();
    let exp = Expect::pinned(&sha256(&body), 1000);
    // Checksum mismatch: nothing is kept.
    srv.file("/wrong.bin", &bytes(1000, 7));
    let dest = dir.path().join("wrong.bin");
    let e = c.download(&srv.url("/wrong.bin"), &dest, &exp, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("checksum mismatch"));
    assert!(!dest.exists() && !part_path(&dest).exists());
    // Wrong size announced: refused before the body is read.
    srv.file("/big.bin", &bytes(5000, 8));
    let e = c.download(&srv.url("/big.bin"), &dir.path().join("big.bin"), &exp, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("expects 1000"), "{e:#}");
    // An HTML page where a file was expected (login or confirmation pages).
    srv.route("/page", Route::File { body: b"<html>".to_vec(), etag: None, ranges: true, content_type: "text/html; charset=utf-8" });
    let e = c.download(&srv.url("/page"), &dir.path().join("p"), &Expect { sha256: None, size: None, max_bytes: 100, allow_html: false }, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("web page"));
    // 404 is not retried.
    srv.clear_hits();
    let e = c.download(&srv.url("/missing"), &dir.path().join("m"), &exp, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("404"));
    assert_eq!(srv.hits_for("/missing").len(), 1);
    // 503 is retried, then given up.
    srv.route("/busy", Route::Status(503));
    let e = c.download(&srv.url("/busy"), &dir.path().join("b"), &exp, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("failed 3 times"), "{e:#}");
    assert_eq!(srv.hits_for("/busy").len(), 3);
    // Streaming limit for unpinned downloads.
    srv.file("/stream.bin", &bytes(10_000, 9));
    let e = c
        .download(&srv.url("/stream.bin"), &dir.path().join("s"), &Expect { sha256: None, size: None, max_bytes: 4096, allow_html: false }, &mut no_progress())
        .unwrap_err();
    assert!(format!("{e:#}").contains("limit"), "{e:#}");
}

#[test]
fn redirects_are_checked_hop_by_hop() {
    let srv = Server::start();
    let body = bytes(2048, 10);
    srv.file("/real.bin", &body);
    srv.route("/go", Route::Redirect("/real.bin".into()));
    srv.route("/evil", Route::Redirect("https://evil.example.com/x.bin".into()));
    srv.route("/downgrade", Route::Redirect("http://github.com/x.bin".into()));
    let dir = tempfile::tempdir().unwrap();
    let c = client();
    let exp = Expect::pinned(&sha256(&body), 2048);
    let d = c.download(&srv.url("/go"), &dir.path().join("ok.bin"), &exp, &mut no_progress()).unwrap();
    assert!(d.final_url.ends_with("/real.bin"));
    let e = c.download(&srv.url("/evil"), &dir.path().join("e.bin"), &exp, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("not in the allowlist"), "{e:#}");
    let e = c.download(&srv.url("/downgrade"), &dir.path().join("d.bin"), &exp, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("non-HTTPS"), "{e:#}");
    // Not on the allowlist from the start.
    let e = c.download("https://example.org/x", &dir.path().join("x"), &exp, &mut no_progress()).unwrap_err();
    assert!(format!("{e:#}").contains("allowlist"));
    assert!(fs::read_dir(dir.path()).unwrap().all(|e| e.unwrap().file_name() == "ok.bin"));
}

#[test]
fn a_complete_part_file_is_not_downloaded_again() {
    let srv = Server::start();
    let body = bytes(4096, 11);
    srv.file("/done.bin", &body);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("done.bin");
    let url = srv.url("/done.bin");
    interrupted(&dest, &url, &sha256(&body), &body, None);
    let d = client().download(&url, &dest, &Expect::pinned(&sha256(&body), 4096), &mut no_progress()).unwrap();
    assert_eq!(d.resumed_from, 4096);
    assert!(srv.hits().is_empty(), "no request was needed");
}
