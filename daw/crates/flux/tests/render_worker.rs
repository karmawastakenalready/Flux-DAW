//! Offline SFZ renders run in worker processes (`flux render-worker`), so a render doesn't depend on what
//! the process rendered before: sfizz shares one random generator between all its synths.

use flux_core::{Command, Project, TimeMap, commands};
use flux_engine::render;
use flux_engine::worker::{self, WorkerCommand};
use serde_json::json;
use std::path::{Path, PathBuf};

/// A test fixture, not an instrument: sfizz's built-in sine with a random level per note, so renders show
/// the random generator's state.
const RANDOM_SFZ: &str = "<region> sample=*sine amp_random=12 ampeg_release=0.05\n";

fn project(sfz: &Path) -> Project {
    let mut p = Project::new("worker", 120.0, 2);
    let notes: Vec<_> = (0..8).map(|i| json!({"beat": i as f64 * 0.5, "dur": 0.4, "pitch": 60 + i})).collect();
    let cmds = json!([
        {"op": "add_track", "name": "Random", "instrument": {"kind": "sfz", "path": sfz.to_string_lossy()}},
        {"op": "add_notes", "track": "Random", "notes": notes},
    ]);
    for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
        commands::apply(&mut p, &c).unwrap();
    }
    p
}

fn fixture(name: &str, text: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flux-render-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join(name);
    std::fs::write(&f, text).unwrap();
    f
}

#[test]
fn sfz_renders_dont_depend_on_what_the_process_rendered_before() {
    if !flux_engine::paths::sfizz_dll().is_file() {
        eprintln!("skipped: needs sfizz ({})", flux_engine::paths::sfizz_dll().display());
        return;
    }
    let p = project(&fixture("random.sfz", RANDOM_SFZ));
    let (tm, t) = (TimeMap::new(&p), &p.tracks[0]);
    let frames = p.sample_rate as usize * 3;

    // in one process the generator carries on, so the same render changes (otherwise this test proves nothing)
    let first = render::render_instrument_here(t, &p, &tm, frames).unwrap().0;
    let again = render::render_instrument_here(t, &p, &tm, frames).unwrap().0;
    assert!(first.0.iter().any(|x| x.abs() > 1e-3), "the fixture should sound");
    assert_ne!(first, again, "sfizz no longer shares its random generator between synths: the worker may not be needed");

    worker::set_command(WorkerCommand { exe: PathBuf::from(env!("CARGO_BIN_EXE_flux")), args: vec![] });
    let (a, note) = render::render_instrument(t, &p, &tm, frames).unwrap();
    assert_eq!(note, "sfizz");
    assert_eq!(a.0.len(), frames);
    let _ = render::render_instrument_here(t, &p, &tm, frames).unwrap(); // moves this process's generator on
    let (b, _) = render::render_instrument(t, &p, &tm, frames).unwrap();
    assert!(a == b, "two renders of the same track differ");
    assert_eq!(a, first, "a worker render is a first render");

    // errors come back with the worker's message
    let broken = project(&fixture("missing.sfz", "<region> sample=does-not-exist.wav\n#include \"nope.sfz\"\n"));
    let missing = project(Path::new("Z:/no/such/instrument.sfz"));
    let e = render::render_instrument(&missing.tracks[0], &missing, &tm, frames).unwrap_err().to_string();
    assert!(e.contains("could not load"), "{e}");
    let _ = render::render_instrument(&broken.tracks[0], &broken, &tm, frames); // sfizz may load it with no regions: it mustn't hang
}
