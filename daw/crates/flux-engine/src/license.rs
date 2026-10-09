//! Instruments whose licenses limit how their sound may leave Flux.
//!
//! Spectrasonics (Omnisphere, Keyscape, Trilian, Stylus RMX) allows its sounds in game music as part of
//! original compositions, but not "distributing your music in a multitrack format that completely
//! exposes tracks created using our samples". A full mix is fine; a stem that is only (or nearly only)
//! a Spectrasonics part is not. So Flux never writes such a part on its own: per-track stems skip it,
//! and a layer stem is refused unless other instruments carry a fair share of it
//! (`MAX_RESTRICTED_SHARE`).

use crate::render::TrackStat;
use crate::{paths, vst};
use flux_core::{Instrument, Track};
use std::collections::BTreeMap;

pub struct Restriction {
    /// The vendor as its plugins report it.
    pub vendor: &'static str,
    /// Product names (lower case, without spaces), to recognize a plugin from its file name.
    pub products: &'static [&'static str],
}

pub const RESTRICTED: &[Restriction] =
    &[Restriction { vendor: "Spectrasonics", products: &["omnisphere", "keyscape", "trilian", "stylusrmx"] }];

/// Largest share of a layer stem's energy that restricted parts may have. Above it the stem is
/// effectively that part alone.
pub const MAX_RESTRICTED_SHARE: f64 = 0.8;

/// The license restriction on a track's instrument, if any. The vendor comes from the plugin itself
/// (once loaded) or its moduleinfo.json; the file name is the fallback.
pub fn of_track(track: &Track) -> Option<&'static Restriction> {
    let Instrument::Vst3 { path, .. } = &track.instrument else { return None };
    if crate::rig::is_rig(path) {
        return None;
    }
    let path = paths::resolve(path);
    let vendor = vst::vendor_of(&path);
    let name = path.file_stem().map(|s| s.to_string_lossy().to_lowercase().replace([' ', '-', '_'], "")).unwrap_or_default();
    RESTRICTED
        .iter()
        .find(|r| vendor.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(r.vendor)) || r.products.iter().any(|p| name.contains(p)))
}

/// Why a track can't be written out on its own, for reports.
pub fn solo_stem_refusal(vendor: &str) -> String {
    format!("not written: the {vendor} license doesn't allow distributing its sounds as an exposed track")
}

/// Layers that would expose restricted parts, each with the reason. `tracks` are a render's track stats.
pub fn exposed_layers(tracks: &[TrackStat], layers: &[String]) -> Vec<(String, String)> {
    // (restricted energy, total energy, restricted track names, vendor)
    let mut by_layer: BTreeMap<&str, (f64, f64, Vec<&str>, &str)> = BTreeMap::new();
    for t in tracks {
        let e = if t.audible { 10f64.powf(t.rms_db / 10.0) } else { 0.0 };
        let l = by_layer.entry(t.layer.as_str()).or_insert((0.0, 0.0, vec![], ""));
        l.1 += e;
        if let Some(v) = &t.restricted {
            l.0 += e;
            l.2.push(&t.name);
            l.3 = v;
        }
    }
    layers
        .iter()
        .filter_map(|name| {
            let (restricted, total, parts, vendor) = by_layer.get(name.as_str())?;
            let share = if *total > 0.0 { restricted / total } else { 0.0 };
            (share > MAX_RESTRICTED_SHARE).then(|| {
                let why = format!(
                    "layer '{name}' is {:.0}% {vendor} ({}): the {vendor} license doesn't allow multitrack music that exposes its \
sounds. Move other instruments into this layer or these parts into another (update_track layer), or export without layers",
                    share * 100.0,
                    parts.join(", ")
                );
                (name.clone(), why)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(name: &str, layer: &str, rms_db: f64, restricted: bool) -> TrackStat {
        TrackStat {
            id: 0,
            name: name.into(),
            engine: String::new(),
            audible: true,
            peak_dbfs: rms_db + 10.0,
            rms_db,
            section_rms_db: vec![],
            warning: None,
            layer: layer.into(),
            restricted: restricted.then(|| "Spectrasonics".into()),
            comp_loud_db: None,
            timbre: None,
        }
    }

    #[test]
    fn a_layer_carried_by_a_restricted_part_is_refused() {
        let tracks = [
            stat("Pad", "pads", -20.0, true),
            stat("Hiss", "pads", -40.0, false),
            stat("Lead", "melody", -20.0, true),
            stat("Strings", "melody", -21.0, false),
            stat("Drums", "base", -18.0, false),
        ];
        let layers: Vec<String> = ["base", "melody", "pads"].map(String::from).into();
        let exposed = exposed_layers(&tracks, &layers);
        assert_eq!(exposed.len(), 1, "{exposed:?}");
        assert_eq!(exposed[0].0, "pads");
        assert!(exposed[0].1.contains("99%") && exposed[0].1.contains("Pad"), "{}", exposed[0].1);
    }

    #[test]
    fn silent_and_muted_parts_dont_hide_a_restricted_one() {
        let mut muted = stat("Muted keys", "pads", -10.0, false);
        muted.audible = false;
        let tracks = [stat("Pad", "pads", -20.0, true), muted, stat("Empty", "pads", -120.0, false)];
        assert_eq!(exposed_layers(&tracks, &["pads".into()]).len(), 1);
    }

    #[test]
    fn products_are_recognized_by_file_name() {
        let t = |path: &str| Track {
            id: 1,
            name: "x".into(),
            color: [0; 3],
            instrument: Instrument::Vst3 { path: path.into(), params: Default::default(), preset: None, preset_name: String::new(), class: None },
            notes: vec![],
            ccs: vec![],
            bends: vec![],
            mixer: Default::default(),
            meta: Default::default(),
            layer: String::new(),
            automation: vec![],
            clips: vec![],
        };
        assert!(of_track(&t("C:/Program Files/Common Files/VST3/Omnisphere.vst3")).is_some());
        assert!(of_track(&t("C:/Program Files/Common Files/VST3/Stylus RMX.vst3")).is_some());
        assert!(of_track(&t("C:/Program Files/Common Files/VST3/Surge XT.vst3")).is_none());
        assert!(of_track(&t("instruments/omnisphere_bass.rig.json")).is_none());
    }
}
