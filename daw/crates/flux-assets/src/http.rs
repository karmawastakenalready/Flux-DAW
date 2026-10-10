//! HTTPS downloads: pure-Rust TLS (ureq + rustls with the ring provider, certificates checked by the operating
//! system's verifier), resumable transfers, checksum verification and a host allowlist that every redirect hop
//! must pass.
//!
//! A download goes to `<dest>.part`, with `<dest>.part.json` remembering the URL, the expected checksum and the
//! server's validators (`ETag` / `Last-Modified`). An interrupted download continues with a `Range` request
//! guarded by `If-Range`, so a file that changed on the server restarts from zero instead of being spliced.
//! Only when the size and SHA-256 match is the part file renamed to `dest`.

use crate::cancel::{Cancel, Cancelled};
use crate::fsutil;
use crate::manifest::{host_allowed, url_host};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ureq::Agent;
use ureq::http::{Response, StatusCode};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// Client settings.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    /// Hosts that may be contacted, including redirect targets (`*.x.y` matches subdomains).
    pub allowed_hosts: Vec<String>,
    /// Allow plain HTTP to 127.0.0.1 / localhost / ::1. For tests only.
    pub allow_loopback_http: bool,
    pub user_agent: String,
    pub connect_timeout: Duration,
    /// Time to wait for response headers.
    pub response_timeout: Duration,
    /// Longest a single attempt may spend receiving a body before it is cut and resumed.
    pub attempt_timeout: Duration,
    /// Attempts in a row that make no progress before giving up.
    pub max_failed_attempts: u32,
    pub max_redirects: u32,
    /// Base delay between retries (doubled each failed attempt, capped at 30 s).
    pub retry_delay: Duration,
    /// Stops a download between chunks; the part file is kept so the next download resumes it.
    pub cancel: Cancel,
}

impl HttpConfig {
    pub fn new(allowed_hosts: Vec<String>) -> Self {
        Self {
            allowed_hosts,
            allow_loopback_http: false,
            user_agent: format!("flux-assets/{} (+https://github.com/; asset fetcher)", env!("CARGO_PKG_VERSION")),
            connect_timeout: Duration::from_secs(30),
            response_timeout: Duration::from_secs(60),
            attempt_timeout: Duration::from_secs(600),
            max_failed_attempts: 4,
            max_redirects: 10,
            retry_delay: Duration::from_secs(1),
            cancel: Cancel::default(),
        }
    }
}

/// What a download must turn out to be.
#[derive(Clone, Debug)]
pub struct Expect {
    /// SHA-256 (hex) the file must have.
    pub sha256: Option<String>,
    /// Exact size in bytes.
    pub size: Option<u64>,
    /// Hard ceiling, enforced while streaming.
    pub max_bytes: u64,
    /// Accept `text/html` responses (only for web pages; files are never HTML).
    pub allow_html: bool,
}

impl Expect {
    /// A pinned file: exact size and checksum.
    pub fn pinned(sha256: &str, size: u64) -> Self {
        Self { sha256: Some(sha256.to_ascii_lowercase()), size: Some(size), max_bytes: size, allow_html: false }
    }
}

/// Result of a finished download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Downloaded {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    /// Bytes that were already in the part file when this call started.
    pub resumed_from: u64,
    /// The URL that finally served the file (after redirects).
    pub final_url: String,
    /// Requests made, including retries.
    pub requests: u32,
}

/// What a server says about a URL, learned from a one-byte range request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub final_url: String,
    pub status: u16,
    /// Full size of the resource, if the server told us.
    pub total_size: Option<u64>,
    pub accepts_ranges: bool,
    pub content_type: Option<String>,
}

/// Progress callback: bytes on disk so far, total if known.
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64, Option<u64>);

#[derive(Debug, Default, Deserialize, Serialize)]
struct PartMeta {
    url: String,
    expected_sha256: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    total: Option<u64>,
}

/// Why an attempt failed.
enum Failure {
    /// Worth another attempt (network trouble, 5xx, truncated body).
    Retry(anyhow::Error),
    /// Will not get better by retrying (404, checksum mismatch, policy violation).
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Fatal(e)
    }
}

/// An HTTPS client bound to a host allowlist.
pub struct HttpClient {
    cfg: HttpConfig,
    agent: Agent,
}

impl HttpClient {
    pub fn new(cfg: HttpConfig) -> Result<Self> {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let config = Agent::config_builder()
            .tls_config(tls)
            .https_only(!cfg.allow_loopback_http)
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(cfg.user_agent.as_str())
            .accept_encoding("identity")
            .timeout_connect(Some(cfg.connect_timeout))
            .timeout_recv_response(Some(cfg.response_timeout))
            .timeout_recv_body(Some(cfg.attempt_timeout))
            .build();
        Ok(Self { cfg, agent: Agent::new_with_config(config) })
    }

    pub fn config(&self) -> &HttpConfig {
        &self.cfg
    }

    /// Fail unless `url` may be contacted: HTTPS on an allowed host (or loopback HTTP when enabled for tests).
    pub fn check_url(&self, url: &str) -> Result<()> {
        let (host, https) = url_host(url)?;
        let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1");
        if self.cfg.allow_loopback_http && loopback {
            return Ok(());
        }
        if !https {
            bail!("refusing non-HTTPS URL {url}");
        }
        if !host_allowed(&self.cfg.allowed_hosts, &host) {
            bail!("refusing {url}: host '{host}' is not in the allowlist (official sources only)");
        }
        Ok(())
    }

    /// GET with manual redirects; every hop is checked against the allowlist.
    fn get(&self, url: &str, headers: &[(&str, String)]) -> Result<(Response<ureq::Body>, String), Failure> {
        let mut current = url.to_string();
        for _ in 0..=self.cfg.max_redirects {
            self.check_url(&current)?;
            let mut req = self.agent.get(&current);
            for (k, v) in headers {
                req = req.header(*k, v.as_str());
            }
            let resp = req.call().map_err(|e| classify(e, &current))?;
            let st = resp.status();
            if st.is_redirection() && st != StatusCode::NOT_MODIFIED {
                let loc = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| Failure::Fatal(anyhow!("{current} redirected ({st}) without a Location")))?;
                current = resolve_location(&current, loc).map_err(Failure::Fatal)?;
                continue;
            }
            return Ok((resp, current));
        }
        Err(Failure::Fatal(anyhow!("too many redirects starting at {url}")))
    }

    /// Learn size and range support of a URL with a one-byte request (nothing is saved).
    pub fn probe(&self, url: &str) -> Result<Probe> {
        let (resp, final_url) = self.get(url, &[("Range", "bytes=0-0".into())]).map_err(failure_error)?;
        let status = resp.status().as_u16();
        let h = resp.headers();
        let content_type = header(h, "content-type");
        let (total_size, accepts_ranges) = match status {
            206 => (header(h, "content-range").and_then(|v| parse_content_range(&v).map(|r| r.2)).flatten(), true),
            200 => (header(h, "content-length").and_then(|v| v.parse().ok()), header(h, "accept-ranges").is_some_and(|v| v.contains("bytes"))),
            _ => bail!("{url}: server answered {status}"),
        };
        Ok(Probe { final_url, status, total_size, accepts_ranges, content_type })
    }

    /// Download a small document (license text, web page) into memory, at most `max_bytes`.
    pub fn get_bytes(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        self.get_bytes_if_found(url, max_bytes)?.ok_or_else(|| anyhow!("{url}: server answered 404 Not Found"))
    }

    /// Like [`get_bytes`](Self::get_bytes), but `None` when the server says there is no such document (404).
    pub fn get_bytes_if_found(&self, url: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        let (resp, final_url) = self.get(url, &[]).map_err(failure_error)?;
        let st = resp.status();
        if st == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !st.is_success() {
            bail!("{url}: server answered {st}");
        }
        let mut out = Vec::new();
        resp.into_body()
            .into_reader()
            .take(max_bytes + 1)
            .read_to_end(&mut out)
            .with_context(|| format!("reading {final_url}"))?;
        if out.len() as u64 > max_bytes {
            bail!("{url} is larger than {max_bytes} bytes");
        }
        Ok(Some(out))
    }

    /// Download `url` to `dest`, resuming a previous partial download when possible. See the module docs.
    pub fn download(&self, url: &str, dest: &Path, expect: &Expect, progress: ProgressFn) -> Result<Downloaded> {
        self.check_url(url)?;
        let part = with_suffix(dest, ".part");
        let meta_path = with_suffix(dest, ".part.json");
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut meta = read_meta(&meta_path);
        let start_len = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let stale = meta.as_ref().is_none_or(|m| m.url != url || m.expected_sha256 != expect.sha256);
        if start_len > 0 && stale {
            discard(&part, &meta_path);
            meta = None;
        }
        let resumed_from = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let mut meta = meta.unwrap_or_else(|| PartMeta { url: url.to_string(), expected_sha256: expect.sha256.clone(), ..Default::default() });
        let mut requests = 0u32;
        let mut failed = 0u32;
        let mut last_err = None;
        while failed < self.cfg.max_failed_attempts {
            self.cfg.cancel.check()?;
            let before = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            requests += 1;
            match self.attempt(url, &part, &meta_path, &mut meta, expect, progress) {
                Ok(final_url) => {
                    let (sha256, bytes) = crate::hash::sha256_file(&part)?;
                    if let Some(want) = expect.size
                        && bytes != want
                    {
                        discard(&part, &meta_path);
                        bail!("{url}: downloaded {bytes} bytes but {want} were expected");
                    }
                    if let Some(want) = &expect.sha256
                        && &sha256 != want
                    {
                        discard(&part, &meta_path);
                        bail!("{url}: checksum mismatch (got sha256 {sha256}, expected {want}); the file was discarded");
                    }
                    fsutil::rename_with_retry(&part, dest)?;
                    let _ = fs::remove_file(&meta_path);
                    return Ok(Downloaded { path: dest.to_path_buf(), bytes, sha256, resumed_from, final_url, requests });
                }
                Err(Failure::Fatal(e)) => return Err(e),
                Err(Failure::Retry(e)) => {
                    let after = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                    if after > before {
                        failed = 0; // progress was made; keep going
                    } else {
                        failed += 1;
                    }
                    last_err = Some(e);
                    if failed < self.cfg.max_failed_attempts {
                        let delay = self.cfg.retry_delay.saturating_mul(1 << failed.min(5)).min(Duration::from_secs(30));
                        let until = Instant::now() + delay;
                        while Instant::now() < until {
                            self.cfg.cancel.check()?;
                            std::thread::sleep((until - Instant::now()).min(Duration::from_millis(100)));
                        }
                    }
                }
            }
        }
        let e = last_err.unwrap_or_else(|| anyhow!("no attempt was made"));
        Err(e.context(format!(
            "downloading {url} failed {} times in a row; run the command again to resume from {}",
            self.cfg.max_failed_attempts,
            part.display()
        )))
    }

    /// One request: append to the part file. Returns the final URL when the body ended normally.
    fn attempt(
        &self,
        url: &str,
        part: &Path,
        meta_path: &Path,
        meta: &mut PartMeta,
        expect: &Expect,
        progress: ProgressFn,
    ) -> Result<String, Failure> {
        let mut offset = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if let Some(size) = expect.size
            && offset > size
        {
            discard(part, meta_path);
            offset = 0;
        }
        if expect.size.is_some_and(|s| s == offset && s > 0) {
            return Ok(url.to_string()); // complete already; the caller verifies the checksum
        }
        let mut headers = Vec::new();
        if offset > 0 {
            headers.push(("Range", format!("bytes={offset}-")));
            // If-Range needs a strong validator; otherwise fall back to the date.
            if let Some(et) = meta.etag.as_ref().filter(|e| !e.starts_with("W/")) {
                headers.push(("If-Range", et.clone()));
            } else if let Some(lm) = &meta.last_modified {
                headers.push(("If-Range", lm.clone()));
            }
        }
        let (resp, final_url) = self.get(url, &headers)?;
        let st = resp.status().as_u16();
        let h = resp.headers().clone();
        let mut append = false;
        let total = match st {
            200 => {
                // Fresh start (or the server ignored the range / the file changed).
                header(&h, "content-length").and_then(|v| v.parse::<u64>().ok())
            }
            206 => {
                let cr = header(&h, "content-range").ok_or_else(|| Failure::Retry(anyhow!("{final_url}: 206 without Content-Range")))?;
                let (start, _end, total) = parse_content_range(&cr).ok_or_else(|| Failure::Fatal(anyhow!("{final_url}: bad Content-Range {cr:?}")))?;
                if start != offset {
                    discard(part, meta_path);
                    return Err(Failure::Retry(anyhow!("{final_url}: server resumed at {start}, not {offset}; restarting")));
                }
                append = true;
                total
            }
            416 => {
                // Range not satisfiable: our part file is complete or bigger than the resource.
                if expect.size.is_none_or(|s| s == offset) && offset > 0 {
                    return Ok(final_url);
                }
                discard(part, meta_path);
                return Err(Failure::Retry(anyhow!("{final_url}: server rejected the resume; restarting")));
            }
            408 | 425 | 429 | 500 | 502 | 503 | 504 => {
                return Err(Failure::Retry(anyhow!("{final_url}: server answered {st}")));
            }
            _ => return Err(Failure::Fatal(anyhow!("{final_url}: server answered {st}"))),
        };
        if !expect.allow_html
            && let Some(ct) = header(&h, "content-type")
            && ct.to_ascii_lowercase().starts_with("text/html")
        {
            return Err(Failure::Fatal(anyhow!(
                "{final_url} returned a web page instead of the file (a confirmation or login page?)"
            )));
        }
        let full = total;
        if let (Some(t), Some(want)) = (full, expect.size)
            && t != want
        {
            return Err(Failure::Fatal(anyhow!("{final_url}: server reports {t} bytes, the manifest expects {want}")));
        }
        if let Some(t) = full
            && t > expect.max_bytes
        {
            return Err(Failure::Fatal(anyhow!("{final_url}: {t} bytes is above the limit of {} bytes", expect.max_bytes)));
        }
        meta.etag = header(&h, "etag");
        meta.last_modified = header(&h, "last-modified");
        meta.total = full;
        write_meta(meta_path, meta).map_err(Failure::Fatal)?;

        let mut file = if append {
            OpenOptions::new().append(true).open(part)
        } else {
            File::create(part)
        }
        .with_context(|| format!("opening {}", part.display()))?;
        let mut done = if append { offset } else { 0 };
        let mut reader = resp.into_body().into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        let mut last_report = Instant::now();
        progress(done, full);
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = file.sync_all();
                    return Err(Failure::Retry(anyhow!(e).context(format!("connection to {final_url} broke after {done} bytes"))));
                }
            };
            if done + n as u64 > expect.max_bytes {
                drop(file);
                discard(part, meta_path);
                return Err(Failure::Fatal(anyhow!("{final_url}: more than {} bytes; aborted", expect.max_bytes)));
            }
            file.write_all(&buf[..n]).with_context(|| format!("writing {}", part.display()))?;
            done += n as u64;
            if self.cfg.cancel.is_cancelled() {
                let _ = file.sync_all();
                return Err(Failure::Fatal(anyhow::Error::new(Cancelled)));
            }
            if last_report.elapsed() >= Duration::from_millis(200) {
                progress(done, full);
                last_report = Instant::now();
            }
        }
        file.sync_all().with_context(|| format!("flushing {}", part.display()))?;
        progress(done, full);
        if let Some(t) = full
            && done < t
        {
            return Err(Failure::Retry(anyhow!("{final_url}: connection closed after {done} of {t} bytes")));
        }
        Ok(final_url)
    }
}

/// The `.part` file a download of `dest` uses while in progress.
pub fn part_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".part")
}

fn failure_error(f: Failure) -> anyhow::Error {
    match f {
        Failure::Retry(e) | Failure::Fatal(e) => e,
    }
}

fn classify(e: ureq::Error, url: &str) -> Failure {
    use ureq::Error as E;
    let retry = matches!(e, E::Io(_) | E::Timeout(_) | E::ConnectionFailed | E::HostNotFound | E::Protocol(_));
    let err = anyhow!(e).context(format!("requesting {url}"));
    if retry { Failure::Retry(err) } else { Failure::Fatal(err) }
}

fn header(h: &ureq::http::HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string())
}

/// `bytes START-END/TOTAL` (TOTAL may be `*`).
fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let total = if total.trim() == "*" { None } else { Some(total.trim().parse().ok()?) };
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total))
}

/// Resolve a `Location` header against the URL that sent it.
fn resolve_location(base: &str, loc: &str) -> Result<String> {
    if loc.starts_with("https://") || loc.starts_with("http://") {
        return Ok(loc.to_string());
    }
    let uri: ureq::http::Uri = base.parse().with_context(|| format!("bad URL {base}"))?;
    let scheme = uri.scheme_str().unwrap_or("https");
    let auth = uri.authority().map(|a| a.as_str()).context("redirect from a URL without host")?;
    if let Some(rest) = loc.strip_prefix("//") {
        return Ok(format!("{scheme}://{rest}"));
    }
    if loc.starts_with('/') {
        return Ok(format!("{scheme}://{auth}{loc}"));
    }
    let path = uri.path();
    let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    Ok(format!("{scheme}://{auth}{dir}{loc}"))
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn read_meta(p: &Path) -> Option<PartMeta> {
    serde_json::from_slice(&fs::read(p).ok()?).ok()
}

fn write_meta(p: &Path, m: &PartMeta) -> Result<()> {
    fsutil::atomic_write(p, &serde_json::to_vec_pretty(m)?)
}

fn discard(part: &Path, meta: &Path) {
    let _ = fs::remove_file(part);
    let _ = fs::remove_file(meta);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range("bytes 0-0/1343285011"), Some((0, 0, Some(1_343_285_011))));
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        assert_eq!(parse_content_range("items 1-2/3"), None);
    }

    #[test]
    fn locations() {
        let b = "https://github.com/a/b/releases/download/1/x.zip";
        assert_eq!(resolve_location(b, "https://objects.githubusercontent.com/y").unwrap(), "https://objects.githubusercontent.com/y");
        assert_eq!(resolve_location(b, "/c/d").unwrap(), "https://github.com/c/d");
        assert_eq!(resolve_location(b, "//cdn.example/z").unwrap(), "https://cdn.example/z");
        assert_eq!(resolve_location(b, "y.zip").unwrap(), "https://github.com/a/b/releases/download/1/y.zip");
    }

    #[test]
    fn url_policy() {
        let c = HttpClient::new(HttpConfig::new(vec!["github.com".into()])).unwrap();
        assert!(c.check_url("https://github.com/x").is_ok());
        assert!(c.check_url("http://github.com/x").is_err());
        assert!(c.check_url("https://example.com/x").is_err());
        assert!(c.check_url("http://127.0.0.1:8080/x").is_err(), "loopback HTTP only when enabled");
        let mut cfg = HttpConfig::new(vec![]);
        cfg.allow_loopback_http = true;
        let t = HttpClient::new(cfg).unwrap();
        assert!(t.check_url("http://127.0.0.1:8080/x").is_ok());
        assert!(t.check_url("http://10.0.0.1/x").is_err());
    }
}
