//! Packs: how the manifest's assets are offered to people installing Flux. A pack groups assets that belong
//! together (Piano, Orchestra, Drums...), in a section of the installer (the engine, instruments, plugins);
//! a selection (Minimal, Recommended) names the assets to tick for a quick choice. Assets in no pack are for
//! developers (test plugins, the retired Python pipeline) and never offered.
//!
//! ```toml
//! [[pack]]
//! id = "piano"
//! name = "Piano"
//! description = "A concert grand, sampled note by note at 16 velocities."
//! assets = ["salamander-grand-piano"]
//!
//! [[pack]]
//! id = "synths"
//! name = "Synths & textures"
//! description = "..."
//! assets = ["karoryfer-cowsynth", "decentsampler-bottles"]
//! needs = ["decent-sampler"]          # packs that must be there too for every part to play
//!
//! [[selection]]
//! id = "minimal"
//! name = "Minimal"
//! description = "A small set to start with."
//! assets = ["vcsl", "sonic-pi-samples"]
//! ```

use crate::manifest::{Asset, Loaded, Source};
use crate::workspace::Workspace;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Where a pack is offered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Section {
    /// Needed by Flux itself; always installed.
    Core,
    /// Sampled instruments, amp captures.
    #[default]
    Instruments,
    /// Plugins: open-source ones Flux downloads, and vendor ones the user installs from the maker's site.
    Plugins,
}

/// A group of assets offered together.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    /// Stable identifier, `[a-z0-9][a-z0-9._-]*`.
    pub id: String,
    pub name: String,
    /// One or two sentences for the installer.
    pub description: String,
    #[serde(default)]
    pub section: Section,
    /// The assets, in the order they are listed when the pack is expanded.
    pub assets: Vec<String>,
    /// Other packs every part of this one needs (a Decent Sampler library needs the Decent Sampler plugin).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
}

/// A quick choice of assets (Minimal, Recommended). "Everything" is every pack and needs no entry.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub id: String,
    pub name: String,
    pub description: String,
    pub assets: Vec<String>,
}

/// How an asset gets installed, for the installer's wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    /// Flux downloads it from its official source.
    Download,
    /// The user downloads it in a browser (a sign-in, a click-through, a host that only serves browsers) and
    /// Flux installs it from the downloaded file.
    ByHand,
    /// Installed by the maker's own installer; Flux only finds it.
    External,
}

/// How far a pack is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackState {
    Installed,
    Partly,
    Missing,
}

impl Asset {
    /// How this asset is installed.
    pub fn delivery(&self) -> Delivery {
        if self.sources.iter().any(|s| matches!(s, Source::External(_))) {
            Delivery::External
        } else if self.sources.iter().any(|s| matches!(s, Source::Manual(_)) || matches!(s, Source::Archive(x) if x.by_hand)) {
            Delivery::ByHand
        } else {
            Delivery::Download
        }
    }

    /// Roughly how many bytes it takes to get: download sizes of every source, including files fetched by hand.
    /// `None` when a source does not say.
    pub fn size_hint(&self) -> Option<u64> {
        let mut total = 0;
        for s in &self.sources {
            total += match s {
                Source::Manual(m) => m.files.iter().map(|f| f.size).sum(),
                Source::Vendored(_) | Source::External(_) => 0,
                other => other.download_size()?,
            };
        }
        Some(total)
    }
}

impl Loaded {
    pub fn packs(&self) -> &[Pack] {
        &self.manifest.packs
    }

    pub fn pack(&self, id: &str) -> Option<&Pack> {
        self.manifest.packs.iter().find(|p| p.id == id)
    }

    pub fn selections(&self) -> &[Selection] {
        &self.manifest.selections
    }

    pub fn selection(&self, id: &str) -> Option<&Selection> {
        self.manifest.selections.iter().find(|s| s.id == id)
    }

    /// The pack an asset is offered in.
    pub fn pack_of(&self, asset: &str) -> Option<&Pack> {
        self.manifest.packs.iter().find(|p| p.assets.iter().any(|a| a == asset))
    }

    /// A pack's assets that apply to this platform.
    pub fn pack_assets(&self, p: &Pack) -> Vec<&Asset> {
        p.assets.iter().filter_map(|id| self.asset(id)).filter(|a| a.applies_here()).collect()
    }

    /// The assets of a selection; `everything` is every asset of every pack.
    pub fn selection_assets(&self, id: &str) -> Result<Vec<String>> {
        if id == "everything" {
            return Ok(self.manifest.packs.iter().flat_map(|p| p.assets.iter().cloned()).collect());
        }
        match self.selection(id) {
            Some(s) => Ok(s.assets.clone()),
            None => {
                let known: Vec<&str> = self.manifest.selections.iter().map(|s| s.id.as_str()).chain(["everything"]).collect();
                bail!("no selection '{id}' (choose {})", known.join(", "))
            }
        }
    }

    /// `ids` plus the assets of every pack they need, and the core packs, without repeats, in manifest order.
    pub fn with_needs(&self, ids: &[String]) -> Vec<String> {
        let mut want: HashSet<String> = ids.iter().cloned().collect();
        for p in self.manifest.packs.iter().filter(|p| p.section == Section::Core) {
            want.extend(p.assets.iter().cloned());
        }
        loop {
            let before = want.len();
            let needed: Vec<String> = self
                .manifest
                .packs
                .iter()
                .filter(|p| p.assets.iter().any(|a| want.contains(a)))
                .flat_map(|p| p.needs.iter())
                .filter_map(|n| self.pack(n))
                .flat_map(|n| n.assets.iter().cloned())
                .collect();
            want.extend(needed);
            if want.len() == before {
                break;
            }
        }
        self.manifest.assets.iter().filter(|a| want.contains(&a.id)).map(|a| a.id.clone()).collect()
    }
}

impl Workspace {
    /// How far a pack is installed (assets for other platforms don't count).
    pub fn pack_state(&self, p: &Pack) -> PackState {
        let assets = self.loaded.pack_assets(p);
        let n = assets.iter().filter(|a| self.is_installed(a)).count();
        match n {
            0 if !assets.is_empty() => PackState::Missing,
            n if n == assets.len() => PackState::Installed,
            _ => PackState::Partly,
        }
    }
}

/// Check the packs and selections against the assets.
pub(crate) fn validate(l: &Loaded) -> Result<()> {
    let m = &l.manifest;
    let mut pack_ids = HashSet::new();
    let mut packed: HashSet<&str> = HashSet::new();
    for p in &m.packs {
        if !crate::manifest::valid_id(&p.id) {
            bail!("pack id '{}' must match [a-z0-9][a-z0-9._-]*", p.id);
        }
        if !pack_ids.insert(p.id.as_str()) {
            bail!("pack id '{}' is used twice", p.id);
        }
        if p.name.trim().is_empty() || p.description.trim().is_empty() {
            bail!("pack '{}': name and description must not be empty", p.id);
        }
        if p.assets.is_empty() {
            bail!("pack '{}' has no assets", p.id);
        }
        for id in &p.assets {
            let Some(a) = l.asset(id) else { bail!("pack '{}': no asset '{id}'", p.id) };
            if !packed.insert(id.as_str()) {
                bail!("asset '{id}' is in two packs (pack '{}' and an earlier one)", p.id);
            }
            if p.section == Section::Core && a.delivery() != crate::packs::Delivery::Download {
                bail!("pack '{}' is installed without asking, so '{id}' must be something Flux can download", p.id);
            }
        }
    }
    for p in &m.packs {
        for n in &p.needs {
            if !pack_ids.contains(n.as_str()) || n == &p.id {
                bail!("pack '{}' needs '{n}', which is not another pack", p.id);
            }
        }
    }
    let mut sel_ids = HashSet::new();
    for s in &m.selections {
        if !crate::manifest::valid_id(&s.id) || s.id == "everything" {
            bail!("selection id '{}' must match [a-z0-9][a-z0-9._-]* and not be 'everything'", s.id);
        }
        if !sel_ids.insert(s.id.as_str()) {
            bail!("selection id '{}' is used twice", s.id);
        }
        for id in &s.assets {
            if !packed.contains(id.as_str()) {
                bail!("selection '{}': '{id}' is not an asset of any pack", s.id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::manifest::parse;

    const HEAD: &str = r#"
schema = 1
[policy]
allowed_hosts = ["github.com"]

[[asset]]
id = "engine"
name = "Engine"
kind = "tool"
scope = "tool"
path = "tools/engine"
version = "1"
license = { id = "MIT" }
[[asset.source]]
type = "archive"
url = "https://github.com/x/engine.zip"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 10
format = "zip"

[[asset]]
id = "piano"
name = "Piano"
kind = "sample-library"
scope = "audio"
path = "instruments/piano"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "git"
url = "https://github.com/x/piano"
rev = "0123456789012345678901234567890123456789"
download_size = "700M"

[[asset]]
id = "bottles"
name = "Bottles"
kind = "sample-library"
scope = "audio"
path = "instruments/bottles"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "manual"
page = "https://github.com/x/bottles"
instructions = "Download it."
[[asset.source.file]]
path = "a.wav"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 5

[[asset]]
id = "player"
name = "Player"
kind = "plugin"
scope = "tool"
version = "1"
license = { id = "MIT" }
[[asset.source]]
type = "external"
page = "https://github.com/x/player"
instructions = "Run its installer."
"#;

    const PACKS: &str = r#"
[[pack]]
id = "core"
name = "Engine"
description = "Plays the instruments."
section = "core"
assets = ["engine"]

[[pack]]
id = "keys"
name = "Keys"
description = "A piano."
assets = ["piano"]

[[pack]]
id = "glass"
name = "Glass"
description = "Bottles."
assets = ["bottles"]
needs = ["player"]

[[pack]]
id = "player"
name = "Player"
description = "The plugin."
section = "plugins"
assets = ["player"]

[[selection]]
id = "minimal"
name = "Minimal"
description = "Just keys."
assets = ["piano"]
"#;

    #[test]
    fn packs_selections_and_needs() {
        let l = parse(&format!("{HEAD}{PACKS}")).unwrap();
        assert_eq!(l.packs().len(), 4);
        assert_eq!(l.pack_of("bottles").unwrap().id, "glass");
        assert_eq!(l.selection_assets("minimal").unwrap(), ["piano"]);
        assert_eq!(l.selection_assets("everything").unwrap(), ["engine", "piano", "bottles", "player"]);
        assert!(l.selection_assets("huge").is_err());
        // core packs come along, and so does what a pack needs
        assert_eq!(l.with_needs(&["bottles".into()]), ["engine", "bottles", "player"]);
        assert_eq!(l.with_needs(&[]), ["engine"]);
        let size = |id: &str| l.asset(id).unwrap().size_hint();
        assert_eq!((size("engine"), size("piano"), size("bottles"), size("player")), (Some(10), Some(700_000_000), Some(5), Some(0)));
        use crate::packs::Delivery::*;
        let how = |id: &str| l.asset(id).unwrap().delivery();
        assert_eq!((how("engine"), how("bottles"), how("player")), (Download, ByHand, External));
    }

    #[test]
    fn bad_packs_are_refused() {
        let bad = |packs: &str, why: &str| {
            let e = format!("{:#}", parse(&format!("{HEAD}{packs}")).unwrap_err());
            assert!(e.contains(why), "{e}");
        };
        bad("[[pack]]\nid = \"a\"\nname = \"A\"\ndescription = \"x\"\nassets = [\"nope\"]\n", "no asset 'nope'");
        bad(
            "[[pack]]\nid = \"a\"\nname = \"A\"\ndescription = \"x\"\nassets = [\"piano\"]\n[[pack]]\nid = \"b\"\nname = \"B\"\ndescription = \"x\"\nassets = [\"piano\"]\n",
            "in two packs",
        );
        bad("[[pack]]\nid = \"a\"\nname = \"A\"\ndescription = \"x\"\nsection = \"core\"\nassets = [\"bottles\"]\n", "must be something Flux can download");
        bad("[[pack]]\nid = \"a\"\nname = \"A\"\ndescription = \"x\"\nassets = [\"piano\"]\nneeds = [\"zzz\"]\n", "not another pack");
        bad("[[selection]]\nid = \"s\"\nname = \"S\"\ndescription = \"x\"\nassets = [\"piano\"]\n", "not an asset of any pack");
        bad("[[selection]]\nid = \"everything\"\nname = \"S\"\ndescription = \"x\"\nassets = []\n", "not be 'everything'");
    }
}
