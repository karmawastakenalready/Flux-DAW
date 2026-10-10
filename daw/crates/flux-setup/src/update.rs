//! Looking for a newer Flux on its GitHub releases page: at most once a day and only while the user allows it
//! (`check_updates` in the settings). Flux shows a notice; nothing is downloaded or installed until the user
//! clicks Update, and then only the release's Setup, checked against the size and checksum GitHub lists for it.

use anyhow::{Context, Result, bail};
use flux_assets::cancel::Cancel;
use flux_assets::http::{Expect, HttpClient, HttpConfig};
use flux_engine::settings::Settings;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The GitHub repository Flux is released from (`FLUX_UPDATE_REPO` overrides it, for testing a fork).
pub const REPO: &str = "karmawastakenalready/Flux-DAW";

/// Seconds between checks.
pub const INTERVAL: u64 = 24 * 60 * 60;

pub fn repo() -> String {
    std::env::var("FLUX_UPDATE_REPO").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| REPO.to_string())
}

/// A published release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Release {
    /// The git tag (`v0.2.0`).
    pub tag: String,
    pub name: String,
    /// The release's page.
    pub page: String,
    /// Its notes (Markdown).
    pub notes: String,
    /// Its Setup program (`FluxSetup-<version>.exe`).
    pub setup: Option<SetupFile>,
}

/// A release's Setup program.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetupFile {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// SHA-256 as GitHub computed it on upload.
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn client(cancel: &Cancel) -> Result<HttpClient> {
    let mut cfg = HttpConfig::new(vec!["api.github.com".into(), "github.com".into(), "*.githubusercontent.com".into()]);
    cfg.user_agent = format!("Flux/{} (update check)", env!("CARGO_PKG_VERSION"));
    cfg.cancel = cancel.clone();
    HttpClient::new(cfg)
}

/// The newest full release of `repo`; `None` when it has none (or can't be seen: a private repository).
pub fn latest(repo: &str) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let Some(body) = client(&Cancel::default())?.get_bytes_if_found(&url, 4 << 20)? else { return Ok(None) };
    read_release(&body)
}

fn read_release(body: &[u8]) -> Result<Option<Release>> {
    let r: ApiRelease = serde_json::from_slice(body).context("reading GitHub's answer about the latest release")?;
    if r.draft || r.prerelease {
        return Ok(None);
    }
    let setup = r.assets.into_iter().find(|a| is_setup_name(&a.name)).map(|a| SetupFile {
        sha256: a.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase),
        name: a.name,
        url: a.browser_download_url,
        size: a.size,
    });
    Ok(Some(Release { name: r.name.filter(|n| !n.is_empty()).unwrap_or_else(|| r.tag_name.clone()), tag: r.tag_name, page: r.html_url, notes: r.body.unwrap_or_default(), setup }))
}

fn is_setup_name(n: &str) -> bool {
    let n = n.to_ascii_lowercase();
    n.starts_with("fluxsetup") && n.ends_with(".exe")
}

/// `v1.2.3` / `1.2.3-beta.1` as comparable parts: numbers, then whether it is a full release.
fn parse(v: &str) -> Option<(Vec<u64>, bool, String)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((c, p)) => (c, p.to_string()),
        None => (v, String::new()),
    };
    let nums: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    let mut nums = nums?;
    while nums.len() < 3 {
        nums.push(0);
    }
    Some((nums, pre.is_empty(), pre))
}

/// Whether `candidate` is a later version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Time to look again (allowed, and a day since the last look).
pub fn due(s: &Settings, now: u64) -> bool {
    s.check_updates && s.last_update_check.is_none_or(|t| t > now || now - t >= INTERVAL)
}

/// The release to tell the user about, from what the last check saw (no network).
pub fn notice(s: &Settings, current: &str) -> Option<String> {
    let tag = s.latest_release.as_ref()?;
    (s.check_updates && is_newer(tag, current) && s.skipped_release.as_ref() != Some(tag)).then(|| tag.clone())
}

/// Look for a newer release now and remember what was seen (also when the look fails, so a computer without
/// internet doesn't try on every start). Returns the release to tell the user about.
pub fn check(s: &mut Settings, current: &str, repo: &str, now: u64) -> Result<Option<Release>> {
    s.last_update_check = Some(now);
    let Some(r) = latest(repo)? else { return Ok(None) };
    s.latest_release = Some(r.tag.clone());
    let tell = is_newer(&r.tag, current) && s.skipped_release.as_ref() != Some(&r.tag);
    Ok(tell.then_some(r))
}

/// Download `r`'s Setup into `dir`, checked against its size and checksum; `progress` gets bytes so far and total.
pub fn download_setup(r: &Release, dir: &Path, cancel: &Cancel, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<PathBuf> {
    let Some(f) = &r.setup else { bail!("release {} has no Setup to download; get it from {}", r.tag, r.page) };
    if !is_setup_name(&f.name) || f.name.contains(['/', '\\']) {
        bail!("refusing the release file name {:?}", f.name);
    }
    let dest = dir.join(&f.name);
    let expect = match &f.sha256 {
        Some(h) => Expect::pinned(h, f.size),
        None => Expect { sha256: None, size: Some(f.size), max_bytes: f.size, allow_html: false },
    };
    client(cancel)?.download(&f.url, &dest, &expect, progress)?;
    Ok(dest)
}

/// Start a downloaded Setup in update mode; Flux should exit right after so Setup can replace it.
pub fn run_setup(setup: &Path) -> Result<()> {
    std::process::Command::new(setup).arg("--update").spawn().with_context(|| format!("starting {}", setup.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.2.0", "0.2.0-beta.1"), "a release beats its own pre-release");
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }

    #[test]
    fn checks_once_a_day_when_allowed() {
        let mut s = Settings::default();
        assert!(due(&s, 1_000_000));
        s.last_update_check = Some(1_000_000);
        assert!(!due(&s, 1_000_000 + 3600));
        assert!(due(&s, 1_000_000 + INTERVAL));
        assert!(due(&s, 10), "a clock set back doesn't stop checks for good");
        s.check_updates = false;
        assert!(!due(&s, 1_000_000 + INTERVAL));
    }

    #[test]
    fn notices_until_skipped() {
        let mut s = Settings { latest_release: Some("v0.2.0".into()), ..Settings::default() };
        assert_eq!(notice(&s, "0.1.0").as_deref(), Some("v0.2.0"));
        assert_eq!(notice(&s, "0.2.0"), None);
        s.skipped_release = Some("v0.2.0".into());
        assert_eq!(notice(&s, "0.1.0"), None);
        s.latest_release = Some("v0.3.0".into());
        assert_eq!(notice(&s, "0.1.0").as_deref(), Some("v0.3.0"), "skipping one release doesn't skip the next");
    }

    #[test]
    fn reads_github_releases() {
        let json = r#"{"tag_name":"v0.2.0","name":"","html_url":"https://github.com/o/r/releases/tag/v0.2.0","body":"Notes","draft":false,"prerelease":false,
            "assets":[{"name":"THIRD-PARTY-NOTICES.txt","browser_download_url":"https://x/n","size":5},
                      {"name":"FluxSetup-0.2.0.exe","browser_download_url":"https://github.com/o/r/releases/download/v0.2.0/FluxSetup-0.2.0.exe","size":42,"digest":"sha256:ABCD"}]}"#;
        let r = read_release(json.as_bytes()).unwrap().unwrap();
        assert_eq!(r.name, "v0.2.0", "an empty name falls back to the tag");
        let f = r.setup.unwrap();
        assert_eq!((f.name.as_str(), f.size), ("FluxSetup-0.2.0.exe", 42));
        assert_eq!(f.sha256.as_deref(), Some("abcd"));
        assert!(read_release(json.replace("\"prerelease\":false", "\"prerelease\":true").as_bytes()).unwrap().is_none());
    }
}
