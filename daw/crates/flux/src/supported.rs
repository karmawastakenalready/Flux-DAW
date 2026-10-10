//! What Flux supports that this computer lacks: the instrument libraries and plugins of the library's asset
//! manifest (`assets.toml`) that aren't installed, and how the user gets each. Agents pass it on (`list_instruments`,
//! `plugins scan`, the `flux://library` resource): downloading gigabytes or running a maker's installer is the
//! user's call.

use flux_assets::fsutil::fmt_bytes;
use flux_assets::packs::{Delivery, Section};
use flux_assets::workspace::{OpenOptions, Workspace};
use flux_engine::paths;
use flux_setup::job::{self, AssetView, PackView};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The library's packs with what is installed (none when its manifest can't be read).
pub fn packs() -> Vec<PackView> {
    Workspace::open(paths::root(), &OpenOptions::default()).map(|ws| job::packs(&ws)).unwrap_or_default()
}

/// How the user gets `a`.
pub fn how_to_get(a: &AssetView) -> String {
    let page = a.page.as_deref().unwrap_or("its maker's site");
    match a.delivery {
        Delivery::Download => format!("Flux downloads it: tick it in File > Library, or run `flux packs install {}`", a.id),
        Delivery::ByHand => format!("download it in a browser from {page}, then import it: File > Library > Import from Downloads, or `flux packs import`"),
        Delivery::External => format!("install it with its maker's installer from {page}; Flux finds it in the VST3 folders"),
    }
}

/// The libraries of `section` that aren't installed and match `query` (a case-insensitive substring of their id,
/// name, description or pack; empty for all), each once.
pub fn missing(packs: &[PackView], section: Section, query: &str) -> Vec<Value> {
    let q = query.to_lowercase();
    let mut seen = BTreeSet::new();
    packs
        .iter()
        .filter(|p| p.section == section)
        .flat_map(|p| p.assets.iter().map(move |a| (p, a)))
        .filter(|(p, a)| !a.installed && (q.is_empty() || [&a.id, &a.name, &a.description, &p.name].iter().any(|s| s.to_lowercase().contains(&q))))
        .filter(|(_, a)| seen.insert(a.id.clone()))
        .map(|(p, a)| {
            let mut j = json!({"id": a.id, "name": a.name, "pack": p.name, "about": a.description, "license": a.license, "get": how_to_get(a)});
            if let Some(s) = a.size {
                j["download"] = json!(fmt_bytes(s));
            }
            j
        })
        .collect()
}

/// The `flux://library` resource: the library's folder and every pack with what is installed, and for what isn't,
/// how the user gets it.
pub fn library_json(packs: &[PackView]) -> Value {
    let packs: Vec<Value> = packs
        .iter()
        .map(|p| {
            let mut j = serde_json::to_value(p).unwrap_or(Value::Null);
            if let Some(list) = j["assets"].as_array_mut() {
                for (v, a) in list.iter_mut().zip(&p.assets) {
                    if !a.installed {
                        v["get"] = json!(how_to_get(a));
                    }
                }
            }
            j
        })
        .collect();
    json!({
        "folder": paths::root().display().to_string(),
        "packs": packs,
        "note": "installing is the user's call: tell them what a song would gain and how big the download is",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_assets::packs::PackState;

    fn asset(id: &str, delivery: Delivery, installed: bool) -> AssetView {
        AssetView {
            id: id.into(),
            name: id.to_uppercase(),
            description: format!("the {id}"),
            license: "CC0-1.0".into(),
            size: (delivery != Delivery::External).then_some(3 << 20),
            delivery,
            installed,
            page: (delivery != Delivery::Download).then(|| format!("https://example.org/{id}")),
            instructions: None,
        }
    }

    fn pack(id: &str, section: Section, assets: Vec<AssetView>) -> PackView {
        PackView { id: id.into(), name: format!("{id} pack"), description: String::new(), section, state: PackState::Partly, needs: vec![], assets }
    }

    #[test]
    fn lists_what_is_missing_once_with_how_to_get_it() {
        let packs = vec![
            pack("strings", Section::Instruments, vec![asset("cello", Delivery::Download, false), asset("violin", Delivery::Download, true)]),
            pack("guitars", Section::Instruments, vec![asset("rig", Delivery::ByHand, false), asset("cello", Delivery::Download, false)]),
            pack("synths", Section::Plugins, vec![asset("vital", Delivery::External, false)]),
        ];
        let ids = |v: Vec<Value>| v.iter().map(|j| j["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(missing(&packs, Section::Instruments, "")), ["cello", "rig"]);
        assert_eq!(ids(missing(&packs, Section::Instruments, "GUITARS")), ["rig", "cello"]);
        assert_eq!(ids(missing(&packs, Section::Plugins, "")), ["vital"]);
        let m = missing(&packs, Section::Instruments, "cello");
        assert_eq!(m[0]["get"], "Flux downloads it: tick it in File > Library, or run `flux packs install cello`");
        assert_eq!(m[0]["download"], "3.15 MB");
        let v = &missing(&packs, Section::Plugins, "")[0];
        assert!(v["get"].as_str().unwrap().contains("https://example.org/vital") && v.get("download").is_none());
        // the resource: what is installed has no `get`
        let lib = library_json(&packs);
        assert!(lib["packs"][0]["assets"][0]["get"].is_string() && lib["packs"][0]["assets"][1].get("get").is_none());
    }
}
