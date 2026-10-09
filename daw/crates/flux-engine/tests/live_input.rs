//! Notes played in from outside (a MIDI keyboard) on a sampled instrument: heard with the song stopped, where the
//! position stands still, and while it plays; and when the song stops, the notes it started stop with it. Needs
//! sfizz and the VSCO 2 cello ensemble (`flux assets fetch`); skipped otherwise.

use flux_core::{Command, Project, commands};
use flux_engine::live::{LiveEngine, RenderHints};
use flux_engine::paths;
use flux_engine::sfizz::Ev;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CELLO: &str = "instruments/VSCO-2-CE/CelloEnsSusVib.sfz";
const SR: u32 = 48_000;

fn s(t: f64) -> usize {
    (t * SR as f64) as usize
}

fn available() -> bool {
    let ok = paths::sfizz_dll().is_file() && paths::resolve(CELLO).is_file();
    if !ok {
        eprintln!("skipped: needs sfizz and {CELLO}");
    }
    ok
}

/// Two bars at 120 BPM (4 s), dry: the cello holds C3 through the second bar.
fn project() -> Project {
    let mut p = Project::new("live-input", 120.0, 2);
    p.sample_rate = SR;
    let cmds = json!([
        {"op": "add_track", "name": "Cello", "instrument": {"kind": "sfz", "path": CELLO}},
        {"op": "add_notes", "track": "Cello", "notes": [{"bar": 2, "beat": 0, "dur": 4.0, "pitch": "C3", "vel": 100}]},
        {"op": "set_mixer", "track": "Cello", "mixer": {"reverb_send": 0}},
    ]);
    for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn rms_db(x: &[f32]) -> f64 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).max(1e-20).log10()
}

/// Left channel of `secs` with the song stopped.
fn stopped(e: &mut LiveEngine, secs: f64) -> Vec<f32> {
    let (mut l, mut r) = (vec![0f32; s(secs)], vec![0f32; s(secs)]);
    assert_eq!(e.render_input(&mut l, &mut r), l.len());
    l
}

/// Left channel of `secs` of the song.
fn playing(e: &mut LiveEngine, secs: f64) -> Vec<f32> {
    let n = s(secs);
    let (mut l, mut r, mut pos) = (vec![0f32; n], vec![0f32; n], vec![0u32; n]);
    assert_eq!(e.render(&mut l, &mut r, &mut pos), n);
    l
}

#[test]
fn notes_played_in_sound_with_the_song_stopped_or_playing() {
    if !available() {
        return;
    }
    let p = project();
    let id = p.tracks[0].id;
    let mut e = LiveEngine::new(Arc::new(p), RenderHints::default());
    let t0 = Instant::now();
    while e.loading() > 0 && t0.elapsed() < Duration::from_secs(60) {
        e.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(e.status()[0].1, "live");

    // stopped: silence until a key goes down, the note while it is held, then its release dies away
    let at = e.position();
    let quiet = rms_db(&stopped(&mut e, 0.3));
    assert!(quiet < -90.0, "{quiet:.1} dB before any key");
    e.input(id, Ev::On(48, 100));
    let held = rms_db(&stopped(&mut e, 0.6)[s(0.2)..]);
    assert!(held > -45.0, "{held:.1} dB while the key is held");
    e.input(id, Ev::Off(48));
    let after = rms_db(&stopped(&mut e, 2.0)[s(1.5)..]);
    assert!(after < held - 40.0, "{after:.1} dB after the key is let go, {held:.1} dB while it was held");
    assert_eq!(e.position(), at, "the song stands still");

    // playing: the first bar has no notes, so what sounds is the key
    e.seek(0);
    let quiet = rms_db(&playing(&mut e, 0.5));
    assert!(quiet < -90.0, "{quiet:.1} dB in the empty bar");
    e.input(id, Ev::On(55, 100));
    let held = rms_db(&playing(&mut e, 0.5)[s(0.2)..]);
    assert!(held > -45.0, "{held:.1} dB while the key is held");
    e.input(id, Ev::Off(55));
    // on into the second bar's note (2 s to 4 s); the song stops in the middle of it, and so does the note
    let note = rms_db(&playing(&mut e, 2.5)[s(2.0)..]);
    assert!(note > -45.0, "{note:.1} dB in the timeline's note");
    let after = rms_db(&stopped(&mut e, 2.0)[s(1.5)..]);
    assert!(after < -80.0, "{after:.1} dB after the song stopped");
}
