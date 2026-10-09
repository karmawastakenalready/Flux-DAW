//! License knowledge: the licenses we know by SPDX id, what each allows, recognising license texts on disk,
//! and the commercial-use policy for a Roblox game.
//!
//! The policy (owner's rule): sound content that ends up in the game may only come from public-domain,
//! permissive or attribution-only licenses. Non-commercial, share-alike, no-derivatives and copyleft content is
//! blocked; freeware and proprietary content needs the owner's explicit approval, recorded in the manifest.
//! Tools that only help make the music are not shipped, so copyleft tools are fine as long as nothing from
//! them is bundled or linked.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;

/// What a license allows, from most to least permissive for our purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Family {
    /// No conditions at all (CC0, Unlicense).
    PublicDomain,
    /// Keep a notice when redistributing (MIT, BSD, ISC, Apache-2.0, OFL, Zlib).
    Permissive,
    /// Credit must be given wherever the work is used (CC BY).
    Attribution,
    /// Free to use, including commercially, but the files themselves may not be redistributed or modified.
    Freeware,
    /// Changes to the files must stay under the same license (LGPL, MPL).
    WeakCopyleft,
    /// Derived works must use the same license (GPL).
    StrongCopyleft,
    /// Like strong copyleft, also triggered by network use (AGPL).
    NetworkCopyleft,
    /// Adaptations must be shared under the same license (CC BY-SA).
    ShareAlike,
    /// No adaptations (CC BY-ND).
    NoDerivatives,
    /// No commercial use (CC BY-NC).
    NonCommercial,
    /// A paid or otherwise restricted license.
    Proprietary,
    /// We could not tell.
    Unknown,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::PublicDomain => "public-domain",
            Family::Permissive => "permissive",
            Family::Attribution => "attribution",
            Family::Freeware => "freeware",
            Family::WeakCopyleft => "weak-copyleft",
            Family::StrongCopyleft => "strong-copyleft",
            Family::NetworkCopyleft => "network-copyleft",
            Family::ShareAlike => "share-alike",
            Family::NoDerivatives => "no-derivatives",
            Family::NonCommercial => "non-commercial",
            Family::Proprietary => "proprietary",
            Family::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Family {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an asset is used, which decides how strict the policy is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// Its sound can end up in the game's music (samples, SoundFonts, amp captures, synth plugins' content).
    Audio,
    /// A program used to make the music; it is not shipped.
    Tool,
    /// Bundled inside the Flux application (fonts); not part of the game.
    App,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Audio => "audio",
            Scope::Tool => "tool",
            Scope::App => "app",
        }
    }
}

/// A license we know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LicenseInfo {
    /// SPDX id (or `LicenseRef-...` for custom licenses).
    pub id: String,
    pub name: String,
    pub family: Family,
    /// Official text or deed.
    pub url: Option<String>,
    /// Whether every use must credit the author (CC BY), as opposed to keeping a notice with copies.
    pub attribution_required: bool,
}

struct Known {
    id: &'static str,
    name: &'static str,
    family: Family,
    url: &'static str,
    /// Detection: every phrase in one group must appear (normalised text); any group may match.
    any_of: &'static [&'static [&'static str]],
    /// Detection fails if any of these appear.
    none_of: &'static [&'static str],
}

const KNOWN: &[Known] = &[
    Known {
        id: "CC0-1.0",
        name: "Creative Commons Zero v1.0 Universal",
        family: Family::PublicDomain,
        url: "https://creativecommons.org/publicdomain/zero/1.0/",
        any_of: &[&["cc0 1 0 universal"], &["creativecommons org publicdomain zero 1 0"]],
        none_of: &[],
    },
    Known {
        id: "Unlicense",
        name: "The Unlicense",
        family: Family::PublicDomain,
        url: "https://unlicense.org/",
        any_of: &[&["this is free and unencumbered software released into the public domain"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-3.0",
        name: "Creative Commons Attribution 3.0 Unported",
        family: Family::Attribution,
        url: "https://creativecommons.org/licenses/by/3.0/",
        any_of: &[&["attribution 3 0 unported"], &["creativecommons org licenses by 3 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-4.0",
        name: "Creative Commons Attribution 4.0 International",
        family: Family::Attribution,
        url: "https://creativecommons.org/licenses/by/4.0/",
        any_of: &[&["attribution 4 0 international"], &["creativecommons org licenses by 4 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-SA-3.0",
        name: "Creative Commons Attribution Share Alike 3.0 Unported",
        family: Family::ShareAlike,
        url: "https://creativecommons.org/licenses/by-sa/3.0/",
        any_of: &[&["attribution sharealike 3 0"], &["creativecommons org licenses by sa 3 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-SA-4.0",
        name: "Creative Commons Attribution Share Alike 4.0 International",
        family: Family::ShareAlike,
        url: "https://creativecommons.org/licenses/by-sa/4.0/",
        any_of: &[&["attribution sharealike 4 0"], &["creativecommons org licenses by sa 4 0"], &["cc by sa 4 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-ND-4.0",
        name: "Creative Commons Attribution No Derivatives 4.0 International",
        family: Family::NoDerivatives,
        url: "https://creativecommons.org/licenses/by-nd/4.0/",
        any_of: &[&["attribution noderivatives 4 0"], &["creativecommons org licenses by nd 4 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-NC-3.0",
        name: "Creative Commons Attribution Non Commercial 3.0 Unported",
        family: Family::NonCommercial,
        url: "https://creativecommons.org/licenses/by-nc/3.0/",
        any_of: &[&["attribution noncommercial 3 0"], &["creativecommons org licenses by nc 3 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-NC-4.0",
        name: "Creative Commons Attribution Non Commercial 4.0 International",
        family: Family::NonCommercial,
        url: "https://creativecommons.org/licenses/by-nc/4.0/",
        any_of: &[&["attribution noncommercial 4 0"], &["creativecommons org licenses by nc 4 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-NC-SA-4.0",
        name: "Creative Commons Attribution Non Commercial Share Alike 4.0 International",
        family: Family::NonCommercial,
        url: "https://creativecommons.org/licenses/by-nc-sa/4.0/",
        any_of: &[&["attribution noncommercial sharealike 4 0"], &["creativecommons org licenses by nc sa 4 0"]],
        none_of: &[],
    },
    Known {
        id: "CC-BY-NC-ND-4.0",
        name: "Creative Commons Attribution Non Commercial No Derivatives 4.0 International",
        family: Family::NonCommercial,
        url: "https://creativecommons.org/licenses/by-nc-nd/4.0/",
        any_of: &[&["attribution noncommercial noderivatives 4 0"], &["creativecommons org licenses by nc nd 4 0"]],
        none_of: &[],
    },
    Known {
        id: "MIT",
        name: "MIT License",
        family: Family::Permissive,
        url: "https://opensource.org/license/mit",
        any_of: &[&["permission is hereby granted free of charge to any person obtaining a copy"]],
        none_of: &[],
    },
    Known {
        id: "BSD-3-Clause",
        name: "BSD 3-Clause \"New\" or \"Revised\" License",
        family: Family::Permissive,
        url: "https://opensource.org/license/bsd-3-clause",
        any_of: &[&["redistribution and use in source and binary forms", "neither the name"]],
        none_of: &[],
    },
    Known {
        id: "BSD-2-Clause",
        name: "BSD 2-Clause \"Simplified\" License",
        family: Family::Permissive,
        url: "https://opensource.org/license/bsd-2-clause",
        any_of: &[&["redistribution and use in source and binary forms"]],
        none_of: &["neither the name"],
    },
    Known {
        id: "ISC",
        name: "ISC License",
        family: Family::Permissive,
        url: "https://opensource.org/license/isc-license-txt",
        any_of: &[&["permission to use copy modify and or distribute this software for any purpose with or without fee"]],
        none_of: &[],
    },
    Known {
        id: "Zlib",
        name: "zlib License",
        family: Family::Permissive,
        url: "https://opensource.org/license/zlib",
        any_of: &[&["altered source versions must be plainly marked", "this notice may not be removed or altered"]],
        none_of: &[],
    },
    Known {
        id: "Apache-2.0",
        name: "Apache License 2.0",
        family: Family::Permissive,
        url: "https://www.apache.org/licenses/LICENSE-2.0",
        any_of: &[&["apache license", "version 2 0"]],
        none_of: &[],
    },
    Known {
        id: "OFL-1.1",
        name: "SIL Open Font License 1.1",
        family: Family::Permissive,
        url: "https://openfontlicense.org/",
        any_of: &[&["sil open font license", "version 1 1"]],
        none_of: &[],
    },
    Known {
        id: "MPL-2.0",
        name: "Mozilla Public License 2.0",
        family: Family::WeakCopyleft,
        url: "https://www.mozilla.org/MPL/2.0/",
        any_of: &[&["mozilla public license version 2 0"]],
        none_of: &[],
    },
    // GNU licenses are recognised by their title and date line only: the GPL's own text recommends the LGPL in its
    // last paragraph, and the LGPL's text refers to the GPL, so looser phrases confuse the two.
    Known {
        id: "LGPL-2.1",
        name: "GNU Lesser General Public License v2.1",
        family: Family::WeakCopyleft,
        url: "https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html",
        any_of: &[&["gnu lesser general public license version 2 1"]],
        none_of: &[],
    },
    Known {
        id: "LGPL-3.0",
        name: "GNU Lesser General Public License v3.0",
        family: Family::WeakCopyleft,
        url: "https://www.gnu.org/licenses/lgpl-3.0.html",
        any_of: &[&["gnu lesser general public license version 3 29 june 2007"]],
        none_of: &[],
    },
    Known {
        id: "GPL-2.0",
        name: "GNU General Public License v2.0",
        family: Family::StrongCopyleft,
        url: "https://www.gnu.org/licenses/old-licenses/gpl-2.0.html",
        any_of: &[&["gnu general public license version 2 june 1991"]],
        none_of: &[],
    },
    Known {
        id: "GPL-3.0",
        name: "GNU General Public License v3.0",
        family: Family::StrongCopyleft,
        url: "https://www.gnu.org/licenses/gpl-3.0.html",
        any_of: &[&["gnu general public license version 3 29 june 2007"]],
        none_of: &[],
    },
    Known {
        id: "AGPL-3.0",
        name: "GNU Affero General Public License v3.0",
        family: Family::NetworkCopyleft,
        url: "https://www.gnu.org/licenses/agpl-3.0.html",
        any_of: &[&["gnu affero general public license version 3 19 november 2007"]],
        none_of: &[],
    },
];

/// A license that has no SPDX id, declared in the manifest (`[[license]]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomLicense {
    /// Must start with `LicenseRef-`.
    pub id: String,
    pub name: String,
    pub family: Family,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub attribution_required: bool,
    /// Phrases that must all appear in a license text for it to be recognised as this license.
    #[serde(default, rename = "match", skip_serializing_if = "Vec::is_empty")]
    pub match_all: Vec<String>,
    /// What the terms say, in one or two sentences, for the generated license overview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Known licenses plus the custom ones from a manifest.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    custom: Vec<CustomLicense>,
}

impl Registry {
    pub fn new(custom: Vec<CustomLicense>) -> Result<Self> {
        for c in &custom {
            if !c.id.starts_with("LicenseRef-") || c.id.len() <= "LicenseRef-".len() {
                bail!("custom license id '{}' must start with 'LicenseRef-'", c.id);
            }
            if !c.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.') {
                bail!("custom license id '{}' may only contain letters, digits, '-' and '.'", c.id);
            }
            if KNOWN.iter().any(|k| k.id == c.id) || custom.iter().filter(|o| o.id == c.id).count() > 1 {
                bail!("license id '{}' is defined twice", c.id);
            }
        }
        Ok(Self { custom })
    }

    pub fn custom(&self) -> &[CustomLicense] {
        &self.custom
    }

    /// Look up a single license id (an SPDX id, optionally with `-only`/`-or-later`, or a custom `LicenseRef-`).
    pub fn lookup(&self, id: &str) -> Option<LicenseInfo> {
        if let Some(c) = self.custom.iter().find(|c| c.id == id) {
            return Some(LicenseInfo {
                id: c.id.clone(),
                name: c.name.clone(),
                family: c.family,
                url: c.url.clone(),
                attribution_required: c.attribution_required,
            });
        }
        let base = base_id(id);
        KNOWN.iter().find(|k| k.id == base).map(|k| LicenseInfo {
            id: id.to_string(),
            name: match id.strip_prefix(base) {
                Some("-or-later") => format!("{} or later", k.name),
                Some("-only") => format!("{} only", k.name),
                _ => k.name.to_string(),
            },
            family: k.family,
            url: Some(k.url.to_string()),
            attribution_required: k.family == Family::Attribution,
        })
    }

    /// Resolve an SPDX expression of single ids joined by `OR` (we may pick the most permissive) or `AND`
    /// (all apply, so the most restrictive counts). Unknown ids are errors.
    pub fn resolve(&self, expr: &str) -> Result<Resolved> {
        let expr = expr.trim();
        if expr.contains('(') || expr.contains(')') {
            bail!("license expression '{expr}': parentheses are not supported; use one id, or ids joined by OR / AND");
        }
        let has_or = expr.contains(" OR ");
        let has_and = expr.contains(" AND ");
        if has_or && has_and {
            bail!("license expression '{expr}' mixes OR and AND");
        }
        let parts: Vec<&str> = if has_or {
            expr.split(" OR ").map(str::trim).collect()
        } else {
            expr.split(" AND ").map(str::trim).collect()
        };
        let mut infos = Vec::new();
        for p in parts {
            match self.lookup(p) {
                Some(i) => infos.push(i),
                None => bail!("unknown license id '{p}' (use an SPDX id or declare a LicenseRef- in [[license]])"),
            }
        }
        let pick = if has_or { infos.iter().min_by_key(|i| i.family) } else { infos.iter().max_by_key(|i| i.family) };
        let family = pick.map_or(Family::Unknown, |i| i.family);
        let attribution_required = if has_or {
            pick.is_some_and(|i| i.attribution_required)
        } else {
            infos.iter().any(|i| i.attribution_required)
        };
        Ok(Resolved { expression: expr.to_string(), family, attribution_required, parts: infos })
    }

    /// License ids whose text appears in `text`, in table order (custom licenses first).
    pub fn detect(&self, text: &str) -> Vec<String> {
        let norm = normalize(text);
        let mut out = Vec::new();
        for c in &self.custom {
            if !c.match_all.is_empty() && c.match_all.iter().all(|m| norm.contains(&normalize(m))) {
                out.push(c.id.clone());
            }
        }
        for k in KNOWN {
            let hit = k.any_of.iter().any(|group| group.iter().all(|p| norm.contains(p)));
            if hit && !k.none_of.iter().any(|p| norm.contains(p)) {
                out.push(k.id.to_string());
            }
        }
        out
    }
}

/// A resolved license expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub expression: String,
    /// The family that governs use (see [`Registry::resolve`]).
    pub family: Family,
    pub attribution_required: bool,
    pub parts: Vec<LicenseInfo>,
}

impl Resolved {
    /// Whether a detected id (from [`Registry::detect`]) is one of this expression's licenses.
    pub fn accepts_detected(&self, detected: &str) -> bool {
        self.parts.iter().any(|p| base_id(&p.id) == base_id(detected))
    }
}

/// An SPDX id without the `-only` / `-or-later` suffix.
pub fn base_id(id: &str) -> &str {
    id.strip_suffix("-or-later").or_else(|| id.strip_suffix("-only")).unwrap_or(id)
}

/// Lowercase, every run of non-alphanumeric characters turned into one space.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = true;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The body of the Markdown (`#`) or plain-text section whose heading contains `heading` (case-insensitive),
/// up to the next heading of the same or a higher level. `None` if there is no such heading.
pub fn extract_section(text: &str, heading: &str) -> Option<String> {
    let want = heading.trim().to_lowercase();
    let lines: Vec<&str> = text.lines().collect();
    let level = |l: &str| {
        let t = l.trim_start();
        let n = t.bytes().take_while(|&b| b == b'#').count();
        (n > 0 && t.as_bytes().get(n) == Some(&b' ')).then_some(n)
    };
    let start = lines.iter().position(|l| level(l).is_some() && l.to_lowercase().contains(&want))?;
    let lvl = level(lines[start]).unwrap_or(1);
    let end = lines[start + 1..]
        .iter()
        .position(|l| level(l).is_some_and(|n| n <= lvl))
        .map_or(lines.len(), |i| start + 1 + i);
    Some(lines[start + 1..end].join("\n"))
}

/// The outcome of checking a license against the policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Fine, nothing to do.
    Allowed,
    /// Fine, and the credit text must ship with the game.
    AllowedWithCredit,
    /// Allowed because the owner approved it (reason recorded in the manifest).
    Approved,
    /// The owner has to decide before this may be used.
    NeedsApproval,
    /// Not allowed for this use.
    Blocked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Allowed => "allowed",
            Verdict::AllowedWithCredit => "allowed (credit required)",
            Verdict::Approved => "approved by owner",
            Verdict::NeedsApproval => "needs owner approval",
            Verdict::Blocked => "blocked",
        }
    }

    /// Whether the asset may be used as things stand.
    pub fn is_usable(self) -> bool {
        matches!(self, Verdict::Allowed | Verdict::AllowedWithCredit | Verdict::Approved)
    }
}

/// Apply the policy to a license family in a scope. `approved` is the owner's recorded approval, which only
/// counts for licenses that need one (it cannot unblock non-commercial or share-alike content).
pub fn assess(family: Family, scope: Scope, approved: bool) -> (Verdict, &'static str) {
    use Family::*;
    let needs = |why| if approved { (Verdict::Approved, why) } else { (Verdict::NeedsApproval, why) };
    match scope {
        Scope::Audio => match family {
            PublicDomain | Permissive => (Verdict::Allowed, "permissive license"),
            Attribution => (Verdict::AllowedWithCredit, "commercial use allowed with credit"),
            Freeware => needs("free to use but not an open license (no redistribution of the files)"),
            Proprietary => needs("proprietary license"),
            NonCommercial => (Verdict::Blocked, "non-commercial license: not allowed in a commercial game"),
            ShareAlike => (Verdict::Blocked, "share-alike license: not allowed by the owner's rules"),
            NoDerivatives => (Verdict::Blocked, "no-derivatives license: sounds may not be adapted"),
            WeakCopyleft | StrongCopyleft | NetworkCopyleft => {
                (Verdict::Blocked, "copyleft license on sound content: not allowed by the owner's rules")
            }
            Unknown => (Verdict::Blocked, "license unknown"),
        },
        Scope::Tool => match family {
            PublicDomain | Permissive | Attribution => (Verdict::Allowed, "tool, not shipped"),
            WeakCopyleft | StrongCopyleft | NetworkCopyleft => {
                (Verdict::Allowed, "copyleft tool, not shipped: never bundle or link it into the game or Flux")
            }
            Freeware => needs("freeware tool"),
            Proprietary => needs("proprietary tool"),
            NonCommercial => needs("non-commercial tool: check that its output may be used commercially"),
            ShareAlike | NoDerivatives => needs("tool under a content license: check the terms"),
            Unknown => needs("tool with an unknown license"),
        },
        Scope::App => match family {
            PublicDomain | Permissive | Attribution => (Verdict::Allowed, "bundled with Flux; keep the notice"),
            WeakCopyleft => needs("weak copyleft inside Flux: keep it dynamically linked and replaceable"),
            Freeware | Proprietary => needs("restricted license inside Flux"),
            _ => (Verdict::Blocked, "license not suitable for bundling inside Flux"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registry {
        Registry::new(vec![CustomLicense {
            id: "LicenseRef-GeneralUser-GS-2.0".into(),
            name: "GeneralUser GS License v2.0".into(),
            family: Family::Permissive,
            url: None,
            attribution_required: false,
            match_all: vec!["GeneralUser GS".into(), "License v2.0".into()],
            summary: None,
        }])
        .unwrap()
    }

    #[test]
    fn detects_common_texts() {
        let r = reg();
        assert_eq!(r.detect("Creative Commons Legal Code\n\nCC0 1.0 Universal\n"), ["CC0-1.0"]);
        assert_eq!(r.detect("Creative Commons Legal Code\n\nAttribution 3.0 Unported\n"), ["CC-BY-3.0"]);
        assert_eq!(r.detect("Attribution-NonCommercial-ShareAlike 4.0 International"), ["CC-BY-NC-SA-4.0"]);
        assert_eq!(r.detect("Permission is hereby granted, free of charge, to any person obtaining a copy"), ["MIT"]);
        assert_eq!(
            r.detect("GNU GENERAL PUBLIC LICENSE\n                       Version 3, 29 June 2007"),
            ["GPL-3.0"]
        );
        // The full GPL-3.0 text ends by recommending the LGPL; it must still read as the GPL.
        let gpl3 = "GNU GENERAL PUBLIC LICENSE\n Version 3, 29 June 2007\n... If this is what you want to do, use the \
                    GNU Lesser General Public License instead of this License.";
        assert_eq!(r.detect(gpl3), ["GPL-3.0"]);
        let lgpl3 = "GNU LESSER GENERAL PUBLIC LICENSE\n Version 3, 29 June 2007\n This version of the GNU Lesser \
                     General Public License incorporates the terms and conditions of version 3 of the GNU General \
                     Public License";
        assert_eq!(r.detect(lgpl3), ["LGPL-3.0"]);
        // LGPL text followed by the GPL it builds on (COPYING.LESSER + COPYING) is both.
        assert_eq!(r.detect(&format!("{lgpl3}\n{gpl3}")), ["LGPL-3.0", "GPL-3.0"]);
        assert_eq!(r.detect("GNU GENERAL PUBLIC LICENSE\nVersion 2, June 1991\n... GNU Library General Public License"), ["GPL-2.0"]);
        assert_eq!(r.detect("GNU AFFERO GENERAL PUBLIC LICENSE\nVersion 3, 19 November 2007"), ["AGPL-3.0"]);
        assert_eq!(r.detect("GNU LESSER GENERAL PUBLIC LICENSE\nVersion 2.1, February 1999"), ["LGPL-2.1"]);
        assert_eq!(r.detect("*** GeneralUser GS v2.0.3 ***\n***      License v2.0     ***"), ["LicenseRef-GeneralUser-GS-2.0"]);
        let bsd2 = "Redistribution and use in source and binary forms, with or without modification, are permitted";
        assert_eq!(r.detect(bsd2), ["BSD-2-Clause"]);
        assert_eq!(r.detect(&format!("{bsd2} Neither the name of the copyright holder")), ["BSD-3-Clause"]);
        assert!(r.detect("All rights reserved.").is_empty());
    }

    #[test]
    fn sections() {
        let md = "# License\n## Main Source Code\nMIT stuff\n## Samples\n\nAll samples are CC0 1.0 Universal.\n### Sub\nmore\n## Docs\nCC BY-SA 4.0";
        let s = extract_section(md, "samples").unwrap();
        assert!(s.contains("CC0 1.0 Universal") && s.contains("more") && !s.contains("BY-SA"));
        assert_eq!(reg().detect(&s), ["CC0-1.0"]);
        assert!(extract_section(md, "Wavetables").is_none());
    }

    #[test]
    fn expressions() {
        let r = reg();
        let x = r.resolve("MIT OR Apache-2.0").unwrap();
        assert_eq!(x.family, Family::Permissive);
        let y = r.resolve("Apache-2.0 AND GPL-3.0-or-later").unwrap();
        assert_eq!(y.family, Family::StrongCopyleft);
        assert!(y.accepts_detected("GPL-3.0"));
        assert_eq!(y.parts[1].name, "GNU General Public License v3.0 or later");
        assert!(r.resolve("CC-BY-3.0").unwrap().attribution_required);
        assert!(r.resolve("Made-Up-1.0").is_err());
        assert!(r.resolve("(MIT OR ISC)").is_err());
        assert!(Registry::new(vec![CustomLicense { id: "Custom".into(), ..reg().custom()[0].clone() }]).is_err());
    }

    #[test]
    fn policy() {
        assert_eq!(assess(Family::PublicDomain, Scope::Audio, false).0, Verdict::Allowed);
        assert_eq!(assess(Family::Attribution, Scope::Audio, false).0, Verdict::AllowedWithCredit);
        assert_eq!(assess(Family::NonCommercial, Scope::Audio, true).0, Verdict::Blocked, "approval cannot unblock NC");
        assert_eq!(assess(Family::ShareAlike, Scope::Audio, false).0, Verdict::Blocked);
        assert_eq!(assess(Family::Freeware, Scope::Audio, false).0, Verdict::NeedsApproval);
        assert_eq!(assess(Family::Freeware, Scope::Audio, true).0, Verdict::Approved);
        assert_eq!(assess(Family::StrongCopyleft, Scope::Tool, false).0, Verdict::Allowed);
        assert_eq!(assess(Family::StrongCopyleft, Scope::Audio, false).0, Verdict::Blocked);
    }

    #[test]
    fn normalises() {
        assert_eq!(normalize("  CC0 1.0\n Universal!! "), "cc0 1 0 universal");
        assert_eq!(normalize("**Attribution-ShareAlike 4.0**"), "attribution sharealike 4 0");
    }
}
