//! A sample player that loads its samples in the background after a state restore (Decent Sampler): a render
//! straight after the plugin loads must sound from the first note, and renders must come out the same every time,
//! round robins and all. Needs Decent Sampler installed, the Bottles library (`flux assets fetch
//! decentsampler-bottles`) and the plugin host built (`cargo build -p flux-vst3`); skipped otherwise.

use flux_core::time::TimeMap;
use flux_core::{Command, Project, commands};
use flux_engine::{paths, vst};
use flux_vst3::State;
use flux_vst3::preset::Preset;
use flux_vst3::vendor::{self, Family, VendorPreset};
use serde_json::json;
use std::path::PathBuf;

const BOTTLES: &str = "instruments/decentsampler-bottles/Bottles/Bottles.dspreset";

fn decent_sampler() -> Option<PathBuf> {
    flux_vst3::scan::standard_dirs().into_iter().map(|d| d.join("DecentSampler.vst3")).find(|p| p.exists())
}

fn available() -> bool {
    let host = std::env::current_exe().unwrap().ancestors().skip(1).take(2).any(|d| d.join("flux-plugin-host.exe").is_file());
    let ok = host && decent_sampler().is_some() && paths::resolve(BOTTLES).is_file();
    if !ok {
        eprintln!("skipped: needs Decent Sampler, {BOTTLES} and flux-plugin-host.exe");
    }
    ok
}

/// Two bars at 96 BPM (5 s): the same note four times (each a different round robin), then a fifth.
fn project(preset: Option<&str>) -> Project {
    let mut p = Project::new("sample-player", 96.0, 2);
    let mut instrument = json!({"kind": "vst3", "path": decent_sampler().unwrap().to_string_lossy()});
    if let Some(s) = preset {
        instrument["preset"] = json!(s);
    }
    let cmds = json!([
        {"op": "add_track", "name": "Bottles", "instrument": instrument},
        {"op": "add_notes", "track": "Bottles", "notes": [
            {"bar": 1, "beat": 0, "dur": 0.75, "pitch": "D4"}, {"bar": 1, "beat": 1, "dur": 0.75, "pitch": "D4"},
            {"bar": 1, "beat": 2, "dur": 0.75, "pitch": "D4"}, {"bar": 1, "beat": 3, "dur": 0.75, "pitch": "D4"},
            {"bar": 2, "beat": 0, "dur": 2.0, "pitch": "A4"}]}
    ]);
    for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

/// The plugin's state with the Bottles library loaded, built from its default state the way `load_preset` does.
fn bottles_state() -> Vec<u8> {
    let p = project(None);
    vst::with_instance(&p.tracks[0], &p, |inst| {
        let current = Preset::from_bytes(&inst.host.state()?)?;
        let library = VendorPreset { name: "Bottles".into(), group: String::new(), path: paths::resolve(BOTTLES), program: None };
        let component = vendor::to_state(Family::DecentSampler, &current.state.component, &library)?;
        Preset { class_id: current.class_id, state: State { component, controller: vec![] }, info: None }.to_bytes()
    })
    .unwrap()
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0f32, |m, v| m.max(v.abs()))
}

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).fold(0f32, |m, (x, y)| m.max((x - y).abs()))
}

#[test]
fn a_sample_player_sounds_from_the_first_note_and_renders_the_same_every_time() {
    if !available() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("flux-sample-player-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let preset = dir.join("bottles.vstpreset");
    std::fs::write(&preset, bottles_state()).unwrap();
    let p = project(Some(&preset.to_string_lossy()));
    let tm = TimeMap::new(&p);
    let sr = p.sample_rate as usize;
    let render = || vst::render(&p.tracks[0], &p, &tm, 5 * sr).unwrap();

    // a fresh instance: the restore loads the library from disk in the background
    vst::close_all();
    let ((cold, _), note) = render();
    assert!(!note.contains("hadn't finished"), "{note}");
    assert!(peak(&cold[..sr / 4]) > 0.01, "the first note should sound (peak {:.6} in the first 0.25 s)", peak(&cold[..sr / 4]));

    // again on the same instance (the library is cached now), and once more: the round robins start over each time
    let ((warm, _), _) = render();
    let ((again, _), _) = render();
    assert_eq!(max_diff(&cold, &warm), 0.0, "a render on a warm instance should match the first");
    assert_eq!(max_diff(&warm, &again), 0.0, "renders should come out the same every time");

    // the four repeats really are different samples (or the test proves nothing about round robins)
    let beat = sr * 60 / 96;
    let first = &warm[..beat / 2];
    let second = &warm[beat..beat + beat / 2];
    assert!(max_diff(first, second) > 1e-3, "repeated notes should play different round robins");

    vst::close_all();
    let _ = std::fs::remove_dir_all(&dir);
}
