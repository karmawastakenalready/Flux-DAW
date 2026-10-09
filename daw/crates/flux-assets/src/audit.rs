//! License audit: checks every asset's license against the policy and against the license text on disk.

use crate::license::{Family, Scope, Verdict, assess, extract_section};
use crate::manifest::Asset;
use crate::usage::Usage;
use crate::workspace::Workspace;
use serde::Serialize;
use std::fs;

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Warning,
    /// Must be resolved before shipping.
    Blocker,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Blocker => "BLOCKER",
        }
    }
}

/// One audit result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub asset: String,
    pub severity: Severity,
    /// Stable machine-readable code, e.g. `license-mismatch`.
    pub code: &'static str,
    pub message: String,
}

/// The license text of an asset as found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LicenseText {
    /// The manifest names no license file.
    NotDeclared,
    /// The asset is not installed, so the file cannot be checked.
    NotInstalled,
    /// Declared but not on disk.
    Missing(String),
    /// Read and checked: the detected license ids.
    Found { file: String, detected: Vec<String> },
    /// The declared section heading is not in the file.
    SectionMissing { file: String, section: String },
}

/// Read the declared license file (and section) and detect which licenses it contains.
pub fn license_text(ws: &Workspace, a: &Asset) -> LicenseText {
    let Some(file) = &a.license.file else { return LicenseText::NotDeclared };
    let Some(dir) = ws.asset_dir(a) else { return LicenseText::NotDeclared };
    if !ws.is_installed(a) {
        return LicenseText::NotInstalled;
    }
    let p = dir.join(file);
    let Ok(bytes) = fs::read(&p) else { return LicenseText::Missing(file.clone()) };
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(4 << 20)]).into_owned();
    let text = match &a.license.section {
        Some(sec) => match extract_section(&text, sec) {
            Some(s) => s,
            None => return LicenseText::SectionMissing { file: file.clone(), section: sec.clone() },
        },
        None => text,
    };
    LicenseText::Found { file: file.clone(), detected: ws.loaded.registry.detect(&text) }
}

/// Policy verdict for an asset.
pub fn verdict(ws: &Workspace, a: &Asset) -> (Verdict, &'static str, Family) {
    let lic = ws.loaded.license(a);
    let (v, why) = assess(lic.family, a.scope, a.license.approved.is_some());
    (v, why, lic.family)
}

/// Audit one asset. `usage` adds the "installed but unused" check.
pub fn audit_asset(ws: &Workspace, a: &Asset, usage: Option<&Usage>) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut push = |severity, code, message: String| out.push(Finding { asset: a.id.clone(), severity, code, message });
    let lic = ws.loaded.license(a);
    let (v, why, family) = verdict(ws, a);
    match v {
        Verdict::Blocked => push(Severity::Blocker, "license-blocked", format!("{}: {why}", a.license.id)),
        Verdict::NeedsApproval => push(
            Severity::Warning,
            "license-needs-approval",
            format!("{} ({family}): {why}; record the owner's decision in license.approved", a.license.id),
        ),
        Verdict::Approved => push(
            Severity::Info,
            "license-approved",
            format!("{} ({family}) approved: {}", a.license.id, a.license.approved.as_deref().unwrap_or("")),
        ),
        Verdict::AllowedWithCredit => push(Severity::Info, "credit-required", format!("{} requires the credit line in the game", a.license.id)),
        Verdict::Allowed => {
            if matches!(family, Family::WeakCopyleft | Family::StrongCopyleft | Family::NetworkCopyleft) && a.scope == Scope::Tool {
                push(Severity::Info, "copyleft-tool", format!("{}: {why}", a.license.id));
            }
        }
    }
    let custom_without_match =
        lic.parts.iter().all(|p| ws.loaded.registry.custom().iter().any(|c| c.id == p.id && c.match_all.is_empty()));
    match license_text(ws, a) {
        LicenseText::NotDeclared => {
            if a.license.text_url.is_none() && a.path.is_some() {
                push(Severity::Warning, "license-text-missing", "no license file in the install and no license.text_url".into());
            } else if a.license.text_url.is_some() && a.path.is_some() {
                push(
                    Severity::Info,
                    "license-not-in-install",
                    "the installed files carry no license text; keep a copy of license.text_url with the release".into(),
                );
            }
        }
        LicenseText::NotInstalled => {}
        LicenseText::Missing(f) => push(Severity::Warning, "license-file-missing", format!("license file {f} is not in the install")),
        LicenseText::SectionMissing { file, section } => {
            push(Severity::Warning, "license-section-missing", format!("{file} has no section '{section}'"))
        }
        LicenseText::Found { file, detected } => {
            if detected.is_empty() {
                if !custom_without_match {
                    push(Severity::Warning, "license-unrecognized", format!("could not recognise the license text in {file}"));
                }
            } else if !detected.iter().any(|d| lic.accepts_detected(d)) {
                let sev = if a.scope == Scope::Audio { Severity::Blocker } else { Severity::Warning };
                push(sev, "license-mismatch", format!("{file} reads as {} but the manifest says {}", detected.join(", "), a.license.id));
            } else if detected.len() > 1 && a.license.section.is_none() {
                push(Severity::Info, "license-multiple", format!("{file} mentions several licenses: {}", detected.join(", ")));
            }
        }
    }
    if let Some(u) = usage
        && a.scope == Scope::Audio
        && ws.is_installed(a)
        && !u.is_used(&a.id)
    {
        push(Severity::Info, "unused", "installed but not referenced by any project, rig, SFZ wrapper or pipeline script".into());
    }
    if let Some(sz) = a.download_size()
        && sz > ws.loaded.manifest.policy.approval_threshold.0
    {
        push(
            Severity::Info,
            "large-download",
            format!(
                "download is {} (over {}): fetching needs the owner's approval (--max-size)",
                crate::fsutil::fmt_bytes(sz),
                crate::fsutil::fmt_bytes(ws.loaded.manifest.policy.approval_threshold.0)
            ),
        );
    }
    out
}

/// Audit every asset; most severe first, then by asset order.
pub fn audit(ws: &Workspace, assets: &[&Asset], usage: Option<&Usage>) -> Vec<Finding> {
    let mut all: Vec<Finding> = assets.iter().flat_map(|a| audit_asset(ws, a, usage)).collect();
    all.sort_by_key(|x| std::cmp::Reverse(x.severity));
    all
}
