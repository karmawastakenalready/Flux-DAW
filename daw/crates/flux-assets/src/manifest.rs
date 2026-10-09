//! The asset manifest (`assets.toml`): every third-party thing the project uses, where it comes from, how to
//! fetch it, how to check it, and under which license.
//!
//! ```toml
//! schema = 1
//!
//! [policy]
//! allowed_hosts = ["github.com", "codeload.github.com", "*.githubusercontent.com"]
//! approval_threshold = "2G"          # bigger downloads need explicit approval
//! download_dir = "tools/_dl"         # where archives are cached
//! usage_roots = ["daw/projects/*.flux.json", "sfz/**/*.sfz"]
//!
//! [[license]]                        # licenses without an SPDX id
//! id = "LicenseRef-GeneralUser-GS-2.0"
//! name = "GeneralUser GS License v2.0"
//! family = "permissive"
//! match = ["GeneralUser GS", "License v2.0"]
//!
//! [[asset]]
//! id = "salamander-grand-piano"
//! name = "Salamander Grand Piano V3"
//! kind = "sample-library"
//! scope = "audio"
//! path = "instruments/SalamanderGrandPiano"
//! version = "3 (git 3382bf9)"
//! authors = ["Alexander Holm"]
//! homepage = "https://github.com/sfzinstruments/SalamanderGrandPiano"
//! license = { id = "CC-BY-3.0", file = "LICENSE" }
//! credit = { text = "Salamander Grand Piano V3 by Alexander Holm, CC BY 3.0" }
//!
//! [[asset.source]]
//! type = "git"
//! url = "https://github.com/sfzinstruments/SalamanderGrandPiano"
//! rev = "3382bf9496bba2486f5ab0de55a264d1dfc38404"
//! branch = "master"
//! download_size = "1.5G"
//! ```
//!
//! Source types: `git` (shallow, optionally sparse checkout of a pinned commit), `archive` (a release zip/tar
//! with a pinned SHA-256, extracted safely), `files` (individual files with pinned SHA-256), `manual` (must be
//! downloaded by hand, e.g. behind a login; files are still verified), `vendored` (kept in this repository) and
//! `external` (installed by another tool, e.g. pip or a vendor installer; only documented).

use crate::extract::ArchiveFormat;
use crate::hash::{is_sha1_hex, is_sha256_hex};
use crate::license::{CustomLicense, Registry, Resolved, Scope};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::path::Path;

/// The only schema version this crate reads and writes.
pub const SCHEMA: u32 = 1;

/// A byte count written either as an integer or as a string such as `"1.5G"` or `"300 MB"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Size(pub u64);

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(Size(n)),
            Raw::Text(s) => crate::fsutil::parse_size(&s).map(Size).map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for Size {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

/// The whole manifest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub credits: CreditsConfig,
    #[serde(default, rename = "license", skip_serializing_if = "Vec::is_empty")]
    pub licenses: Vec<CustomLicense>,
    #[serde(default, rename = "asset")]
    pub assets: Vec<Asset>,
}

/// Download and audit policy.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Hosts downloads may come from (including every redirect hop). `*.example.com` matches subdomains.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Downloads larger than this (per asset) need explicit approval (`--max-size`). Default 2 GB.
    #[serde(default = "default_threshold")]
    pub approval_threshold: Size,
    /// Where downloaded archives are cached, relative to the project root.
    #[serde(default = "default_download_dir")]
    pub download_dir: String,
    /// Files (globs relative to the root) whose references decide whether an asset is in use.
    #[serde(default)]
    pub usage_roots: Vec<String>,
}

fn default_threshold() -> Size {
    Size(2_000_000_000)
}

fn default_download_dir() -> String {
    "tools/_dl".to_string()
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            approval_threshold: default_threshold(),
            download_dir: default_download_dir(),
            usage_roots: Vec::new(),
        }
    }
}

/// Settings for the generated credits and license overview.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreditsConfig {
    /// First lines of the in-game credits text (e.g. the composer line).
    #[serde(default)]
    pub header: Vec<String>,
    /// Paragraph at the top of the generated LICENSES.md.
    #[serde(default)]
    pub intro: Option<String>,
}

/// What an asset is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    SampleLibrary,
    Soundfont,
    AmpCapture,
    Plugin,
    Tool,
    Font,
    PythonPackage,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SampleLibrary => "sample-library",
            Kind::Soundfont => "soundfont",
            Kind::AmpCapture => "amp-capture",
            Kind::Plugin => "plugin",
            Kind::Tool => "tool",
            Kind::Font => "font",
            Kind::PythonPackage => "python-package",
            Kind::Other => "other",
        }
    }
}

/// One third-party asset.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    /// Stable identifier, `[a-z0-9][a-z0-9._-]*`.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub scope: Scope,
    /// Install folder relative to the project root; absent for things installed elsewhere (pip, vendor installers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub version: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// What it is used for, one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional assets are not installed by `fetch --all` (installed ones are kept complete) and missing ones are
    /// not errors.
    #[serde(default)]
    pub optional: bool,
    /// Platforms this asset applies to (`windows`, `macos`, `linux`); empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Paths (relative to the asset folder) that are not part of any source and must not be reported as drift,
    /// e.g. files the user adds. Globs allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    pub license: AssetLicense,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<Credit>,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
    /// Free-form remarks shown in the license overview.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The license of an asset and where its text lives.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetLicense {
    /// SPDX expression: one id, or ids joined by ` OR ` / ` AND `; `LicenseRef-...` for custom licenses.
    pub id: String,
    /// License text inside the asset folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Only this section of `file` applies (for files that list several licenses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Where the license text is published, when the installed files do not include it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_url: Option<String>,
    /// SHA-256 of the document at `text_url`, checked when it is downloaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_sha256: Option<String>,
    /// The owner's recorded decision for licenses that need approval (who, when, why).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<String>,
    /// Remarks about the terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Credit text for the game.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credit {
    /// What the asset provides, e.g. "Piano samples".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The exact credit line.
    pub text: String,
    /// Whether the license requires it. Defaults to what the license says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// Where (part of) an asset comes from.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Source {
    Git(GitSource),
    Archive(ArchiveSource),
    Files(FilesSource),
    Manual(ManualSource),
    Vendored(VendoredSource),
    External(ExternalSource),
}

impl Source {
    pub fn type_name(&self) -> &'static str {
        match self {
            Source::Git(_) => "git",
            Source::Archive(_) => "archive",
            Source::Files(_) => "files",
            Source::Manual(_) => "manual",
            Source::Vendored(_) => "vendored",
            Source::External(_) => "external",
        }
    }

    /// Bytes to download for a fresh install, if known.
    pub fn download_size(&self) -> Option<u64> {
        match self {
            Source::Git(g) => g.download_size.map(|s| s.0),
            Source::Archive(a) => Some(a.size),
            Source::Files(f) => Some(f.files.iter().map(|x| x.size).sum()),
            _ => None,
        }
    }

    /// Whether `fetch` can install this source by itself.
    pub fn is_fetchable(&self) -> bool {
        matches!(self, Source::Git(_) | Source::Archive(_) | Source::Files(_))
    }
}

/// A shallow checkout of one pinned commit.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    pub url: String,
    /// Full 40-digit commit id.
    pub rev: String,
    /// Branch the commit was taken from (informational, and used for the clone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Sparse-checkout patterns; empty = the whole tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sparse: Vec<String>,
    /// Interpret `sparse` as cone-mode directories instead of gitignore-style patterns.
    #[serde(default)]
    pub cone: bool,
    /// Approximate download size, used for prompts and limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_size: Option<Size>,
}

/// A downloadable archive with a pinned checksum.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSource {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub format: ArchiveFormat,
    /// Official page that links to `url` (for people checking provenance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// Cache file name (default: last URL path component).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Existing cached copy relative to the root, if it lives somewhere other than `download_dir/<id>/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// Leading path components to drop from every archive entry.
    #[serde(default)]
    pub strip: usize,
    /// Only extract entries matching one of these globs (after `strip`); empty = everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Sub-folder of the asset folder to extract into.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dest: String,
    /// MD5 that upstream publishes next to the download (provenance record; the pinned SHA-256 is what is checked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_md5: Option<String>,
    /// Entries the system archiver cannot name (RAR/7z only) and that are knowingly skipped; the exact count
    /// must match or extraction fails.
    #[serde(default)]
    pub skipped_entries: usize,
}

/// Individual files downloaded over HTTPS.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilesSource {
    /// URLs of files without an absolute `url` are `base_url` + `/` + `path` (or `url`).
    pub base_url: String,
    /// Sub-folder of the asset folder the files go to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dest: String,
    #[serde(rename = "file")]
    pub files: Vec<FileSpec>,
}

/// One pinned file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    /// Install path relative to the source's `dest`.
    pub path: String,
    /// URL (absolute) or path relative to `base_url`; defaults to `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub sha256: String,
    pub size: u64,
}

/// Something a person has to download by hand (login, licence click-through). Files are still verified.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManualSource {
    pub page: String,
    pub instructions: String,
    #[serde(default, rename = "file", skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PinnedFile>,
}

/// Files kept in this repository.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VendoredSource {
    /// Where the upstream release came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, rename = "file", skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PinnedFile>,
}

/// Installed by another tool (a vendor installer, pip); documented here, and found where `installed` says.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSource {
    pub page: String,
    pub instructions: String,
    /// Where the install is, below a standard folder (`{vst3}/Vital.vst3`; see [`crate::places`]). The first
    /// location is the install itself and decides whether the asset is installed; later ones are content it comes
    /// with (factory presets, impulse responses). Projects that reference a path inside any of them use the asset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed: Vec<String>,
}

/// A file with a known size and checksum, relative to the asset folder.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

impl FilesSource {
    /// Full URL of a file.
    pub fn url_of(&self, f: &FileSpec) -> String {
        let rel = f.url.as_deref().unwrap_or(&f.path);
        if rel.starts_with("https://") || rel.starts_with("http://") {
            return rel.to_string();
        }
        let enc: Vec<String> = rel.split('/').map(percent_encode_segment).collect();
        format!("{}/{}", self.base_url.trim_end_matches('/'), enc.join("/"))
    }
}

/// Percent-encode one URL path segment (RFC 3986 unreserved characters stay as they are).
pub fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Asset {
    /// Whether this asset applies to the platform we run on.
    pub fn applies_here(&self) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|p| p == current_platform())
    }

    /// Total download size of all fetchable sources, if every one is known.
    pub fn download_size(&self) -> Option<u64> {
        self.sources.iter().filter(|s| s.is_fetchable()).map(|s| s.download_size()).sum()
    }

    pub fn git_source(&self) -> Option<&GitSource> {
        self.sources.iter().find_map(|s| if let Source::Git(g) = s { Some(g) } else { None })
    }

    /// Whether `fetch` can install every source of this asset.
    pub fn is_fetchable(&self) -> bool {
        !self.sources.is_empty() && self.sources.iter().all(Source::is_fetchable)
    }
}

/// `windows`, `macos` or `linux`.
pub fn current_platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// A manifest that passed validation, with its license registry.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub manifest: Manifest,
    pub registry: Registry,
    /// Accept `file://` and loopback `http://` URLs. Only for tests with local servers and repositories.
    pub allow_local_urls: bool,
}

impl Loaded {
    pub fn assets(&self) -> &[Asset] {
        &self.manifest.assets
    }

    pub fn asset(&self, id: &str) -> Option<&Asset> {
        self.manifest.assets.iter().find(|a| a.id == id)
    }

    /// The resolved license of an asset (validated at load time, so this cannot fail for loaded manifests).
    pub fn license(&self, a: &Asset) -> Resolved {
        self.registry.resolve(&a.license.id).unwrap_or_else(|_| Resolved {
            expression: a.license.id.clone(),
            family: crate::license::Family::Unknown,
            attribution_required: false,
            parts: vec![],
        })
    }

    /// Assets by id, in manifest order; all assets when `ids` is empty. Unknown ids are an error.
    pub fn select(&self, ids: &[String]) -> Result<Vec<&Asset>> {
        if ids.is_empty() {
            return Ok(self.manifest.assets.iter().collect());
        }
        let mut out = Vec::new();
        for id in ids {
            match self.asset(id) {
                Some(a) => {
                    if !out.iter().any(|x: &&Asset| x.id == a.id) {
                        out.push(a);
                    }
                }
                None => {
                    let close: Vec<&str> = self
                        .manifest
                        .assets
                        .iter()
                        .map(|a| a.id.as_str())
                        .filter(|x| x.contains(id.as_str()) || id.contains(x))
                        .collect();
                    if close.is_empty() {
                        bail!("no asset '{id}' in the manifest (see `flux assets status`)");
                    }
                    bail!("no asset '{id}' in the manifest; did you mean {}?", close.join(", "));
                }
            }
        }
        Ok(out)
    }

    /// Whether downloads from `url` are allowed by the policy (HTTPS and an allowed host).
    pub fn check_url(&self, url: &str) -> Result<()> {
        if self.allow_local_urls && is_local_url(url) {
            return Ok(());
        }
        check_url(&self.manifest.policy.allowed_hosts, url)
    }
}

/// `file://` or plain HTTP to the loopback interface.
fn is_local_url(url: &str) -> bool {
    url.starts_with("file://")
        || ["http://127.0.0.1:", "http://127.0.0.1/", "http://localhost:", "http://localhost/"].iter().any(|p| url.starts_with(p))
}

/// Read and validate a manifest file.
pub fn load(path: &Path) -> Result<Loaded> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

/// Parse and validate manifest text.
pub fn parse(text: &str) -> Result<Loaded> {
    parse_with(text, false)
}

/// [`parse`], optionally accepting local URLs (`file://`, `http://127.0.0.1`) for tests.
pub fn parse_with(text: &str, allow_local_urls: bool) -> Result<Loaded> {
    let manifest: Manifest = toml::from_str(text).context("the manifest is not valid")?;
    let registry = Registry::new(manifest.licenses.clone())?;
    let loaded = Loaded { manifest, registry, allow_local_urls };
    validate(&loaded)?;
    Ok(loaded)
}

fn validate(l: &Loaded) -> Result<()> {
    let m = &l.manifest;
    if m.schema != SCHEMA {
        bail!("manifest schema {} is not supported (this build reads schema {SCHEMA})", m.schema);
    }
    for h in &m.policy.allowed_hosts {
        let bare = h.strip_prefix("*.").unwrap_or(h);
        if bare.is_empty() || !bare.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
            bail!("policy.allowed_hosts: '{h}' is not a host name");
        }
    }
    check_rel_path(&m.policy.download_dir, "policy.download_dir")?;
    let mut ids = HashSet::new();
    let mut paths: Vec<(&str, &str)> = Vec::new();
    for a in &m.assets {
        let ctx = || format!("asset '{}'", a.id);
        if !valid_id(&a.id) {
            bail!("asset id '{}' must match [a-z0-9][a-z0-9._-]*", a.id);
        }
        if !ids.insert(a.id.as_str()) {
            bail!("asset id '{}' is used twice", a.id);
        }
        validate_asset(l, a).with_context(ctx)?;
        if let Some(p) = &a.path {
            paths.push((p.as_str(), a.id.as_str()));
        }
    }
    // Asset folders must not overlap: verifying one would otherwise see the other's files.
    for (i, (p, id)) in paths.iter().enumerate() {
        for (q, other) in &paths[i + 1..] {
            let (pl, ql) = (p.to_ascii_lowercase(), q.to_ascii_lowercase());
            if pl == ql || ql.starts_with(&format!("{pl}/")) || pl.starts_with(&format!("{ql}/")) {
                bail!("assets '{id}' ({p}) and '{other}' ({q}) have overlapping folders");
            }
        }
    }
    Ok(())
}

fn validate_asset(l: &Loaded, a: &Asset) -> Result<()> {
    if a.name.trim().is_empty() || a.version.trim().is_empty() {
        bail!("name and version must not be empty");
    }
    for p in &a.platforms {
        if !matches!(p.as_str(), "windows" | "macos" | "linux") {
            bail!("unknown platform '{p}' (use windows, macos or linux)");
        }
    }
    let lic = l.registry.resolve(&a.license.id).context("license.id")?;
    if lic.attribution_required && a.credit.is_none() {
        bail!("license {} requires attribution, so a [asset.credit] text is required", a.license.id);
    }
    if let Some(f) = &a.license.file {
        check_rel_path(f, "license.file")?;
        if a.path.is_none() {
            bail!("license.file needs an asset path");
        }
    }
    for u in [&a.homepage, &a.license.text_url].into_iter().flatten() {
        if !u.starts_with("https://") && !u.starts_with("http://") {
            bail!("'{u}' is not a web address");
        }
    }
    if let Some(h) = &a.license.text_sha256 {
        if a.license.text_url.is_none() {
            bail!("license.text_sha256 needs license.text_url");
        }
        if !is_sha256_hex(h) {
            bail!("license.text_sha256 must be 64 hex digits");
        }
    }
    if let Some(p) = &a.path {
        check_rel_path(p, "path")?;
    }
    for g in &a.ignore {
        check_rel_path(g, "ignore")?;
    }
    let needs_path = a.sources.iter().any(|s| !matches!(s, Source::External(_)));
    if needs_path && a.path.is_none() {
        bail!("an asset with {} sources needs a path", a.sources.first().map_or("", Source::type_name));
    }
    if a.sources.is_empty() {
        bail!("at least one [[asset.source]] is required");
    }
    for (i, s) in a.sources.iter().enumerate() {
        validate_source(l, s).with_context(|| format!("source {} ({})", i + 1, s.type_name()))?;
        // The source that creates the folder (git, or an archive extracted to its root) must come first.
        let creates_root = matches!(s, Source::Git(_)) || matches!(s, Source::Archive(x) if x.dest.is_empty());
        if creates_root && i > 0 {
            bail!("a {} source that fills the asset folder must be the first source", s.type_name());
        }
    }
    if a.sources.iter().filter(|s| matches!(s, Source::Git(_))).count() > 1 {
        bail!("only one git source per asset");
    }
    Ok(())
}

fn validate_source(l: &Loaded, s: &Source) -> Result<()> {
    match s {
        Source::Git(g) => {
            l.check_url(&g.url)?;
            if !is_sha1_hex(&g.rev) {
                bail!("rev must be a full 40-digit commit id, got '{}'", g.rev);
            }
            if let Some(b) = &g.branch
                && (b.is_empty() || b.starts_with('-') || b.contains(['\0', ' ', '\n', '~', '^', ':', '\\']))
            {
                bail!("'{b}' is not a branch name");
            }
            if g.download_size.is_none() {
                bail!("download_size is required (used for size prompts and limits)");
            }
            for p in &g.sparse {
                if p.trim().is_empty() || p.starts_with('-') || p.contains(['\n', '\r', '\0']) {
                    bail!("bad sparse pattern {p:?}");
                }
            }
            if g.cone {
                for p in &g.sparse {
                    check_rel_path(p, "sparse (cone mode)")?;
                }
            }
        }
        Source::Archive(x) => {
            l.check_url(&x.url)?;
            if !is_sha256_hex(&x.sha256) {
                bail!("sha256 must be 64 hex digits");
            }
            if x.size == 0 {
                bail!("size must be the archive size in bytes");
            }
            if let Some(m) = &x.upstream_md5
                && !(m.len() == 32 && m.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                bail!("upstream_md5 must be 32 hex digits");
            }
            if let Some(f) = &x.file_name {
                check_file_name(f)?;
            }
            if let Some(c) = &x.cache {
                check_rel_path(c, "cache")?;
            }
            if !x.dest.is_empty() {
                check_rel_path(&x.dest, "dest")?;
            }
            for g in &x.include {
                check_rel_path(g, "include")?;
            }
            if x.skipped_entries > 0 && !x.format.is_external() {
                bail!("skipped_entries only applies to RAR and 7z archives");
            }
        }
        Source::Files(f) => {
            l.check_url(&f.base_url)?;
            if !f.dest.is_empty() {
                check_rel_path(&f.dest, "dest")?;
            }
            if f.files.is_empty() {
                bail!("a files source needs at least one [[asset.source.file]]");
            }
            let mut seen = BTreeSet::new();
            for x in &f.files {
                check_rel_path(&x.path, "file.path")?;
                if !seen.insert(x.path.to_ascii_lowercase()) {
                    bail!("file '{}' is listed twice", x.path);
                }
                if !is_sha256_hex(&x.sha256) {
                    bail!("file '{}': sha256 must be 64 hex digits", x.path);
                }
                l.check_url(&f.url_of(x)).with_context(|| format!("file '{}'", x.path))?;
            }
        }
        Source::Manual(m) => {
            check_web(&m.page)?;
            check_pinned(&m.files)?;
        }
        Source::Vendored(v) => check_pinned(&v.files)?,
        Source::External(e) => {
            check_web(&e.page)?;
            for loc in &e.installed {
                check_location(loc).with_context(|| format!("installed '{loc}'"))?;
            }
        }
    }
    Ok(())
}

/// An install location is a standard folder's placeholder and a relative path below it.
fn check_location(loc: &str) -> Result<()> {
    let Some((name, rest)) = crate::places::split(loc) else {
        bail!("must start with a placeholder such as {{vst3}}/ (one of {})", crate::places::NAMES.join(", "));
    };
    if !crate::places::NAMES.contains(&name) {
        bail!("unknown placeholder {{{name}}} (use one of {})", crate::places::NAMES.join(", "));
    }
    check_rel_path(rest, "location")
}

fn check_pinned(files: &[PinnedFile]) -> Result<()> {
    for f in files {
        check_rel_path(&f.path, "file.path")?;
        if !is_sha256_hex(&f.sha256) {
            bail!("file '{}': sha256 must be 64 hex digits", f.path);
        }
    }
    Ok(())
}

fn check_web(u: &str) -> Result<()> {
    if !u.starts_with("https://") {
        bail!("page '{u}' must be an https:// address");
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
}

/// A relative `/`-separated path that stays inside its base: no absolute paths, drive letters, backslashes,
/// `.`/`..` components or empty components.
pub fn check_rel_path(p: &str, what: &str) -> Result<()> {
    if p.is_empty() {
        bail!("{what}: empty path");
    }
    if p.starts_with('/') || p.contains('\\') || p.contains(':') || p.contains('\0') {
        bail!("{what}: '{p}' must be a relative path with '/' separators");
    }
    for c in p.split('/') {
        if c.is_empty() || c == "." || c == ".." {
            bail!("{what}: '{p}' has an empty, '.' or '..' component");
        }
    }
    Ok(())
}

fn check_file_name(f: &str) -> Result<()> {
    check_rel_path(f, "file_name")?;
    if f.contains('/') {
        bail!("file_name '{f}' must not contain '/'");
    }
    Ok(())
}

/// Host part of an `http(s)://` URL, lowercased, plus whether it is HTTPS.
pub fn url_host(url: &str) -> Result<(String, bool)> {
    let uri: ureq::http::Uri = url.parse().with_context(|| format!("'{url}' is not a valid URL"))?;
    let https = match uri.scheme_str() {
        Some("https") => true,
        Some("http") => false,
        _ => bail!("'{url}' must be an http(s) URL"),
    };
    let host = uri.host().with_context(|| format!("'{url}' has no host"))?;
    if url.contains('@') && uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        bail!("'{url}' must not contain credentials");
    }
    Ok((host.to_ascii_lowercase(), https))
}

/// Whether `host` is in the allowlist (`*.x.y` entries match subdomains of `x.y`).
pub fn host_allowed(allowed: &[String], host: &str) -> bool {
    allowed.iter().any(|a| match a.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{}", suffix.to_ascii_lowercase())),
        None => a.eq_ignore_ascii_case(host),
    })
}

/// URL must be HTTPS and on an allowed host.
pub fn check_url(allowed: &[String], url: &str) -> Result<()> {
    let (host, https) = url_host(url)?;
    if !https {
        bail!("'{url}' is not HTTPS; downloads must use HTTPS");
    }
    if !host_allowed(allowed, &host) {
        bail!("host '{host}' of '{url}' is not in policy.allowed_hosts (official sources only)");
    }
    Ok(())
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
schema = 1
[policy]
allowed_hosts = ["github.com", "*.githubusercontent.com"]
approval_threshold = "2G"
usage_roots = ["daw/projects/*.flux.json"]

[[license]]
id = "LicenseRef-Test"
name = "Test License"
family = "freeware"

[[asset]]
id = "piano"
name = "Piano"
kind = "sample-library"
scope = "audio"
path = "instruments/Piano"
version = "1"
license = { id = "CC-BY-3.0", file = "LICENSE" }
credit = { text = "Piano by Someone, CC BY 3.0" }
[[asset.source]]
type = "git"
url = "https://github.com/x/piano"
rev = "3382bf9496bba2486f5ab0de55a264d1dfc38404"
sparse = ["/LICENSE", "/Samples/"]
download_size = "1.5G"
[[asset.source]]
type = "files"
base_url = "https://raw.githubusercontent.com/x/piano/sfz"
[[asset.source.file]]
path = "Piano Map.sfz"
sha256 = "9575028c7a1f589f5770fccc8cff2734566af40cd26ed836944e9a5152688cfe"
size = 10

[[asset]]
id = "thing"
name = "Thing"
kind = "tool"
scope = "tool"
version = "2"
license = { id = "LicenseRef-Test" }
[[asset.source]]
type = "external"
page = "https://example.com/thing"
instructions = "install it"
"#;

    #[test]
    fn parses_and_validates() {
        let l = parse(GOOD).unwrap();
        assert_eq!(l.assets().len(), 2);
        let a = l.asset("piano").unwrap();
        assert_eq!(a.download_size(), Some(1_500_000_010));
        assert_eq!(l.manifest.policy.approval_threshold, Size(2_000_000_000));
        let Source::Files(f) = &a.sources[1] else { panic!() };
        assert_eq!(f.url_of(&f.files[0]), "https://raw.githubusercontent.com/x/piano/sfz/Piano%20Map.sfz");
        assert!(l.select(&["pian".into()]).unwrap_err().to_string().contains("did you mean piano"));
        assert_eq!(l.select(&[]).unwrap().len(), 2);
    }

    fn broken(from: &str, to: &str) -> String {
        assert!(GOOD.contains(from), "test setup: {from}");
        let err = parse(&GOOD.replacen(from, to, 1)).unwrap_err();
        format!("{err:#}")
    }

    #[test]
    fn rejects_bad_manifests() {
        assert!(broken("schema = 1", "schema = 2").contains("schema 2"));
        assert!(broken("https://github.com/x/piano\"", "https://evil.example/x\"").contains("allowed_hosts"));
        assert!(broken("https://github.com/x/piano\"", "http://github.com/x/piano\"").contains("HTTPS"));
        assert!(broken("3382bf9496bba2486f5ab0de55a264d1dfc38404", "master").contains("40-digit"));
        assert!(broken("instruments/Piano", "../Piano").contains("'..'"));
        assert!(broken("instruments/Piano", "C:/Piano").contains("relative"));
        assert!(broken("credit = { text = \"Piano by Someone, CC BY 3.0\" }", "").contains("requires attribution"));
        assert!(broken("id = \"thing\"", "id = \"piano\"").contains("used twice"));
        assert!(broken("CC-BY-3.0", "CC-BY-9.9").contains("unknown license"));
        assert!(broken("kind = \"tool\"", "kind = \"spaceship\"").contains("unknown variant"));
        assert!(broken("path = \"Piano Map.sfz\"", "path = \"../escape.sfz\"").contains("'..'"));
        let overlap = GOOD.replace("version = \"2\"", "version = \"2\"\npath = \"instruments/Piano/sub\"").replace(
            "type = \"external\"",
            "type = \"manual\"",
        );
        assert!(format!("{:#}", parse(&overlap).unwrap_err()).contains("overlapping"));
    }

    #[test]
    fn hosts() {
        let allowed = vec!["github.com".to_string(), "*.githubusercontent.com".to_string()];
        assert!(host_allowed(&allowed, "github.com"));
        assert!(host_allowed(&allowed, "objects.githubusercontent.com"));
        assert!(!host_allowed(&allowed, "githubusercontent.com"));
        assert!(!host_allowed(&allowed, "evilgithub.com"));
        assert!(!host_allowed(&allowed, "github.com.evil.net"));
        assert!(check_url(&allowed, "https://user:pw@github.com/x").is_err());
        assert_eq!(url_host("https://GitHub.com:443/a?b=c").unwrap(), ("github.com".into(), true));
    }
}
