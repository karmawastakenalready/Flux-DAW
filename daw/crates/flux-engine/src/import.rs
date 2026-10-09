//! Importers: the Python pipeline's tracks (meta JSON + MIDI + stems [+ mix.json]) and plain MIDI files.

use anyhow::{bail, Context, Result};
use flux_core::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

struct MidiTrack {
    name: String,
    notes: Vec<Note>,
    ccs: Vec<CcPoint>,
    bends: Vec<BendPoint>,
}

fn parse_midi(path: &Path) -> Result<(Vec<MidiTrack>, Option<f64>)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let smf = midly::Smf::parse(&bytes).map_err(|e| anyhow::anyhow!("MIDI parse error: {e}"))?;
    let ppq = match smf.header.timing {
        midly::Timing::Metrical(t) => t.as_int() as f64,
        _ => bail!("SMPTE-timed MIDI is not supported"),
    };
    let mut out = vec![];
    let mut first_bpm = None;
    for tr in smf.tracks.iter() {
        let mut tick: u64 = 0;
        let mut name = String::new();
        let mut open: BTreeMap<(u8, u8), Vec<(u64, u8)>> = BTreeMap::new();
        let mut t = MidiTrack { name: String::new(), notes: vec![], ccs: vec![], bends: vec![] };
        for ev in tr.iter() {
            tick += ev.delta.as_int() as u64;
            let beat = tick as f64 / ppq;
            match ev.kind {
                midly::TrackEventKind::Meta(midly::MetaMessage::TrackName(n)) => name = String::from_utf8_lossy(n).to_string(),
                midly::TrackEventKind::Meta(midly::MetaMessage::Tempo(us)) => {
                    if first_bpm.is_none() {
                        first_bpm = Some(60_000_000.0 / us.as_int() as f64);
                    }
                }
                midly::TrackEventKind::Midi { channel, message } => {
                    let ch = channel.as_int();
                    match message {
                        midly::MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                            open.entry((ch, key.as_int())).or_default().push((tick, vel.as_int()));
                        }
                        midly::MidiMessage::NoteOn { key, .. } | midly::MidiMessage::NoteOff { key, .. } => {
                            if let Some(v) = open.get_mut(&(ch, key.as_int()))
                                && !v.is_empty() {
                                    let (st, vel) = v.remove(0);
                                    let dur = ((tick - st) as f64 / ppq).max(1e-4);
                                    t.notes.push(Note { start: st as f64 / ppq, dur, pitch: key.as_int(), vel });
                                }
                        }
                        midly::MidiMessage::Controller { controller, value } => {
                            t.ccs.push(CcPoint { beat, cc: controller.as_int(), value: value.as_int() })
                        }
                        midly::MidiMessage::PitchBend { bend } => t.bends.push(BendPoint { beat, value: bend.as_int() }),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        t.name = name;
        if !t.notes.is_empty() || !t.ccs.is_empty() {
            t.notes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
            out.push(t);
        }
    }
    Ok((out, first_bpm))
}

pub fn import_midi(path: &Path) -> Result<Project> {
    let (tracks, bpm) = parse_midi(path)?;
    let bpm = bpm.unwrap_or(120.0);
    let end = tracks.iter().flat_map(|t| t.notes.iter().map(|n| n.start + n.dur)).fold(0f64, f64::max);
    let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or("imported".into());
    let mut p = Project::new(&name, bpm, ((end / 4.0).ceil() as u32).max(1));
    p.looped = false;
    for (i, t) in tracks.into_iter().enumerate() {
        let id = p.next_id;
        p.next_id += 1;
        p.tracks.push(Track {
            id,
            name: if t.name.is_empty() { format!("track {}", i + 1) } else { t.name },
            color: PALETTE[i % PALETTE.len()],
            instrument: Instrument::None,
            notes: t.notes,
            ccs: t.ccs,
            bends: t.bends,
            mixer: Mixer::default(),
            meta: Default::default(),
            layer: String::new(),
        });
    }
    Ok(p)
}

fn eq_from(v: &Value) -> Vec<EqBand> {
    let mut out = vec![];
    if let Some(arr) = v.as_array() {
        for b in arr {
            let kind = match b.get(0).and_then(|x| x.as_str()).unwrap_or("peak") {
                "low_shelf" => EqKind::LowShelf,
                "high_shelf" => EqKind::HighShelf,
                _ => EqKind::Peak,
            };
            out.push(EqBand {
                kind,
                hz: b.get(1).and_then(|x| x.as_f64()).unwrap_or(1000.0),
                db: b.get(2).and_then(|x| x.as_f64()).unwrap_or(0.0),
                q: b.get(3).and_then(|x| x.as_f64()).unwrap_or(0.9),
            });
        }
    }
    out
}

fn mixer_from(cfg: &Value, notes: &mut Vec<String>, name: &str) -> Mixer {
    let f = |k: &str, d: f64| cfg.get(k).and_then(|x| x.as_f64()).unwrap_or(d);
    let mut m = Mixer {
        gain_db: f("gain", 0.0),
        pan: f("pan", 0.0),
        width: f("width", 1.0),
        hpf_hz: f("hpf", 0.0),
        eq: eq_from(cfg.get("eq").unwrap_or(&Value::Null)),
        post_gain_db: f("post_gain", 0.0),
        reverb_send: f("send", 0.0),
        ..Mixer::default()
    };
    if let Some(c) = cfg.get("comp").and_then(|c| c.as_array()) {
        m.comp = Some(Comp {
            threshold_db: c.first().and_then(|x| x.as_f64()).unwrap_or(-4.0),
            relative: true,
            ratio: c.get(1).and_then(|x| x.as_f64()).unwrap_or(2.0),
            attack_ms: 15.0,
            release_ms: 150.0,
            makeup_db: 0.0,
        });
    }
    if let Some(fx) = cfg.get("fx").and_then(|x| x.as_array()) {
        for item in fx {
            match item.get(0).and_then(|x| x.as_str()) {
                Some("distortion") => m.drive_db = item.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0),
                Some("lowpass") => m.lpf_hz = item.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0),
                Some("chorus") => {
                    m.chorus = Some(ChorusFx {
                        rate_hz: item.get(1).and_then(|x| x.as_f64()).unwrap_or(0.5),
                        depth: item.get(2).and_then(|x| x.as_f64()).unwrap_or(0.25),
                        mix: item.get(3).and_then(|x| x.as_f64()).unwrap_or(0.5),
                        centre_delay_ms: 7.0,
                    })
                }
                Some("bitcrush") => m.bitcrush_bits = item.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0),
                Some("clip") => m.clip_db = item.get(1).and_then(|x| x.as_f64()),
                Some("delay") => {
                    m.delay = Some(DelayFx {
                        seconds: item.get(1).and_then(|x| x.as_f64()).unwrap_or(0.3),
                        feedback: item.get(2).and_then(|x| x.as_f64()).unwrap_or(0.3),
                        mix: item.get(3).and_then(|x| x.as_f64()).unwrap_or(0.2),
                    })
                }
                Some(other) => notes.push(format!("{name}: '{other}' effect not available in Flux yet (skipped)")),
                None => {}
            }
        }
    }
    m
}

/// Import a track produced by the Python pipeline: `<title>.json` beside `<title>.mid` and `stems/`.
pub fn import_pipeline(meta_path: &Path) -> Result<Project> {
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(meta_path)?)?;
    let dir = meta_path.parent().unwrap_or(Path::new("."));
    let title = meta["title"].as_str().unwrap_or("imported").to_string();
    let midi = meta_path.with_extension("mid");
    let (mtracks, _) = parse_midi(&midi)?;
    let bars = meta["bars"].as_u64().unwrap_or(16) as u32;
    let mut p = Project::new(&title, meta["bpm"].as_f64().unwrap_or(120.0), bars);
    p.looped = meta["loop"].as_bool().unwrap_or(false);
    p.key = meta["key"].as_str().unwrap_or("").to_string();
    if let Some(tmap) = meta["tempo_map"].as_array() {
        p.tempo = tmap
            .iter()
            .map(|t| TempoPoint { bar: t["bar"].as_u64().unwrap_or(1) as u32, bpm: t["bpm"].as_f64().unwrap_or(120.0) })
            .collect();
    }
    if let Some(secs) = meta["sections"].as_array() {
        for s in secs {
            let bars_s = s["bars"].as_str().unwrap_or("1-1");
            let (a, b) = bars_s.split_once('-').unwrap_or(("1", "1"));
            let a: u32 = a.parse().unwrap_or(1);
            let b: u32 = b.parse().unwrap_or(a);
            p.sections.push(Section { name: s["name"].as_str().unwrap_or("").to_string(), start_bar: a, bars: b - a + 1 });
        }
    }
    let mix: Option<Value> = std::fs::read_to_string(dir.join("mix.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
    let mut warnings = vec![];
    let set = meta["instrument_set"].as_str().unwrap_or("").to_string();
    for (i, t) in mtracks.into_iter().enumerate() {
        let stem = dir.join("stems").join(format!("{}.wav", t.name));
        let instrument = if stem.exists() {
            Instrument::Audio { path: crate::paths::portable(&stem) }
        } else {
            Instrument::None
        };
        let mut meta_map = BTreeMap::new();
        let sfz = crate::paths::root().join("build").join("sfz").join(&set).join(format!("{}.sfz", t.name));
        if sfz.exists() {
            meta_map.insert("sfz".to_string(), crate::paths::portable(&sfz));
        }
        meta_map.insert("imported_from".to_string(), crate::paths::portable(meta_path));
        let mixer = mix
            .as_ref()
            .and_then(|m| m["tracks"].get(&t.name))
            .map(|cfg| mixer_from(cfg, &mut warnings, &t.name))
            .unwrap_or_default();
        let id = p.next_id;
        p.next_id += 1;
        p.tracks.push(Track {
            id,
            name: t.name,
            color: PALETTE[i % PALETTE.len()],
            instrument,
            notes: t.notes,
            ccs: t.ccs,
            bends: t.bends,
            mixer,
            meta: meta_map,
                    layer: String::new(),
        });
    }
    if let Some(m) = &mix {
        if let Some(r) = m.get("reverb") {
            let g = |k: &str, d: f64| r.get(k).and_then(|x| x.as_f64()).unwrap_or(d);
            p.master.reverb = ReverbParams {
                room_size: g("room_size", 0.7),
                damping: g("damping", 0.5),
                predelay_ms: g("predelay_s", 0.02) * 1000.0,
                hpf_hz: g("return_hpf", 220.0),
                lpf_hz: g("return_lpf", 7500.0),
                return_db: g("return_gain_db", 0.0),
            };
        }
        if let Some(eq) = m.get("master_eq") {
            p.master.eq = eq_from(eq);
        }
        if let Some(g) = m.get("glue").and_then(|g| g.as_array()) {
            let x = |i: usize, d: f64| g.get(i).and_then(|v| v.as_f64()).unwrap_or(d);
            p.master.glue = Some(Comp {
                threshold_db: x(0, -3.0),
                relative: true,
                ratio: x(1, 1.8),
                attack_ms: x(2, 30.0),
                release_ms: x(3, 250.0),
                makeup_db: 0.0,
            });
        }
        if let Some(v) = m.get("target_lufs").and_then(|v| v.as_f64()) {
            p.master.target_lufs = v;
        }
        if let Some(v) = m.get("ceiling_dbtp").and_then(|v| v.as_f64()) {
            p.master.ceiling_dbtp = v;
        }
        // sidechain ducking: the pipeline keyed it from exact trigger times (kicks); keep them as a silent
        // trigger track so the ducking stays identical and visible. Fall back to the drum track.
        let tracks_cfg = m["tracks"].as_object();
        let wants_duck = tracks_cfg.is_some_and(|tr| tr.values().any(|c| c.get("duck").is_some()));
        if wants_duck {
            let source = if let Some(trig) = meta["duck_triggers"].as_array() {
                let tm = TimeMap::new(&p);
                let notes: Vec<Note> = trig
                    .iter()
                    .filter_map(|x| x.as_f64())
                    .map(|s| Note { start: tm.beat(s), dur: 0.25, pitch: 36, vel: 100 })
                    .collect();
                let id = p.next_id;
                p.next_id += 1;
                let mut meta_map = BTreeMap::new();
                meta_map.insert("role".to_string(), "silent sidechain trigger: its notes duck other tracks".to_string());
                p.tracks.push(Track {
                    id,
                    name: "sidechain".into(),
                    color: [110, 110, 110],
                    instrument: Instrument::None,
                    notes,
                    ccs: vec![],
                    bends: vec![],
                    mixer: Mixer::default(),
                    meta: meta_map,
                    layer: String::new(),
                });
                Some(id)
            } else {
                p.tracks.iter().find(|t| t.name == "drums" || t.name == "percussion").map(|t| t.id)
            };
            if let (Some(kid), Some(tr)) = (source, tracks_cfg) {
                for t in p.tracks.iter_mut() {
                    if let Some(d) = tr.get(&t.name).and_then(|c| c.get("duck")).and_then(|d| d.as_array()) {
                        t.mixer.duck = Some(Duck {
                            source_track: kid,
                            depth_db: d.first().and_then(|x| x.as_f64()).unwrap_or(4.0),
                            release_ms: d.get(1).and_then(|x| x.as_f64()).unwrap_or(150.0),
                        });
                    }
                }
            }
        }
    } else {
        warnings.push("no mix.json beside the song (run daw/bridge/export_mix.py): tracks start with a flat mix".into());
    }
    if let Some(ts) = meta["timestops"].as_array().filter(|t| !t.is_empty()) {
        let bars: Vec<String> = ts.iter().filter_map(|t| t["bars"].as_str().map(|b| b.split(' ').next().unwrap_or(b).to_string())).collect();
        warnings.push(format!(
            "{} time-stop tape effects (bars {}) are not available in Flux yet: those moments play straight",
            ts.len(),
            bars.join(", ")
        ));
    }
    if !warnings.is_empty() {
        p.notes = format!("Import notes:\n- {}", warnings.join("\n- "));
    }
    Ok(p)
}
