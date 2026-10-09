//! The game-wide loudness check on files: a synthesised game with known defects (see `common::write_game`).

mod common;

use flux_analysis::loudness::{GameOptions, GameReport, check_game};
use std::path::PathBuf;

fn codes(rep: &GameReport, name: &str) -> Vec<&'static str> {
    let t = rep.tracks.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no track {name} in {:?}", rep.tracks.iter().map(|t| &t.name).collect::<Vec<_>>()));
    t.flags.iter().map(|f| f.code).collect()
}

#[test]
fn a_game_directory_is_checked_track_by_track() {
    let dir = tempfile::tempdir().unwrap();
    common::write_game(dir.path());
    let rep = check_game(&[dir.path().to_path_buf()], &GameOptions::default()).unwrap();

    // manifests claim their layer files; the text file and the export report are not candidates
    let names: Vec<&str> = rep.tracks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["arena.wav", "battle_loop.wav", "boss.wav", "explore_loop.wav", "field.wav", "town.wav"]);
    assert_eq!(rep.unreadable.len(), 1, "{:?}", rep.unreadable);
    assert!(rep.unreadable[0].path.ends_with("broken.wav"));

    for (name, want) in common::expected_flags() {
        let mut got = codes(&rep, name);
        // a cut loop also clicks
        if name == "battle_loop.wav" {
            got.retain(|c| *c != "seam_click");
        }
        assert_eq!(got, want, "{name}: {:#?}", rep.tracks.iter().find(|t| t.name == name).unwrap().flags);
    }

    // loop detection: by name, by Flux's export report, by the manifest; never for the one-shot
    let looped: Vec<(&str, bool)> = rep.tracks.iter().map(|t| (t.name.as_str(), t.looped)).collect();
    assert_eq!(
        looped,
        [("arena.wav", true), ("battle_loop.wav", true), ("boss.wav", true), ("explore_loop.wav", true), ("field.wav", true), ("town.wav", false)]
    );
    let seam = |n: &str| rep.tracks.iter().find(|t| t.name == n).unwrap().seam.clone().unwrap();
    // the manifest's tempo gives bar lines; a plain file is compared with its own attacks
    assert_eq!(seam("boss.wav").reference, Some("bar_lines"));
    assert_eq!(seam("explore_loop.wav").reference, Some("onsets"));
    assert!(seam("explore_loop.wav").jump_ok && !seam("battle_loop.wav").jump_ok);

    let boss = rep.tracks.iter().find(|t| t.name == "boss.wav").unwrap();
    let layers = boss.layers.as_ref().unwrap();
    assert_eq!(layers.layers.len(), 2);
    assert!(layers.max_error_db.unwrap() < -120.0, "{layers:?}");
    assert_eq!(layers.manifest_sum_error_db, Some(-150.0));
    assert_eq!(boss.bits, Some(32));
    let town = rep.tracks.iter().find(|t| t.name == "town.wav").unwrap();
    assert_eq!(town.bits, Some(16));
    assert!((town.stats.integrated_lufs.unwrap() + 20.0).abs() < 0.1, "{:?}", town.stats);
    let explore = rep.tracks.iter().find(|t| t.name == "explore_loop.wav").unwrap();
    assert!((explore.stats.integrated_lufs.unwrap() + 15.0).abs() < 0.05, "{:?}", explore.stats);
    assert!(explore.stats.true_peak_dbtp < -1.0 && explore.stats.plr_db.unwrap() > 6.0, "{:?}", explore.stats);

    assert_eq!(rep.summary.tracks, 6);
    assert_eq!(rep.summary.median_lufs, Some(-15.0));
    // errors: arena (unusable layer, layer sum), battle_loop (seam jump), town (target)
    assert_eq!(rep.summary.errors, 4, "{:?}", rep.summary);
    assert!(rep.summary.warnings >= 2, "{:?}", rep.summary);
    // the report is what the CLI and an MCP tool return
    let json = serde_json::to_value(&rep).unwrap();
    assert_eq!(json["tracks"][0]["flags"][0]["severity"], "warning");
    assert_eq!(json["tracks"][0]["flags"][0]["code"], "layer_stale");
}

#[test]
fn loop_detection_can_be_switched_off() {
    let dir = tempfile::tempdir().unwrap();
    common::write_game(dir.path());
    let o = GameOptions { detect_loops: false, ..GameOptions::default() };
    let rep = check_game(&[dir.path().join("explore_loop.wav"), dir.path().join("field.wav"), dir.path().join("boss.layers.json")], &o).unwrap();
    let looped: Vec<(&str, bool)> = rep.tracks.iter().map(|t| (t.name.as_str(), t.looped)).collect();
    // a manifest still says how its master plays
    assert_eq!(looped, [("boss.wav", true), ("explore_loop.wav", false), ("field.wav", false)]);
    assert!(rep.tracks.iter().filter(|t| !t.looped).all(|t| t.seam.is_none()));
}

#[test]
fn files_and_options_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    common::write_game(dir.path());
    let o = GameOptions::default();
    assert!(check_game(&[], &o).is_err());
    let e = check_game(&[dir.path().join("nothing.wav")], &o).unwrap_err();
    assert!(format!("{e:#}").contains("does not exist"), "{e:#}");
    let e = check_game(&[dir.path().join("notes.txt")], &o).unwrap_err();
    assert!(format!("{e:#}").contains("not a WAV, FLAC or .layers.json"), "{e:#}");
    let empty = tempfile::tempdir().unwrap();
    assert!(check_game(&[empty.path().to_path_buf()], &o).is_err());
    // a stricter size limit flags every file
    let small = GameOptions { max_file_bytes: 1000, ..GameOptions::default() };
    let rep = check_game(&[dir.path().join("town.wav")], &small).unwrap();
    assert!(rep.tracks[0].flags.iter().any(|f| f.code == "file_size"));
    // a sub-directory is searched only when asked
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::copy(dir.path().join("town.wav"), sub.join("cave.wav")).unwrap();
    let flat = check_game(&[dir.path().to_path_buf()], &o).unwrap();
    let deep = check_game(&[dir.path().to_path_buf()], &GameOptions { recursive: true, ..o }).unwrap();
    assert_eq!(deep.tracks.len(), flat.tracks.len() + 1);
    let _: PathBuf = deep.tracks.iter().find(|t| t.name == "cave.wav").unwrap().path.clone().unwrap();
}
