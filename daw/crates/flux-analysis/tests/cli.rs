//! The `flux-analyze` command line: output formats and exit codes.

mod common;

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flux-analyze")).args(args).output().expect("running flux-analyze")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn usage_and_bad_arguments() {
    let o = run(&[]);
    assert_eq!(o.status.code(), Some(0));
    assert!(stdout(&o).contains("flux-analyze loudness PATH"));
    let o = run(&["loudness", "--help"]);
    assert_eq!(o.status.code(), Some(0));
    for bad in [&["nonsense"][..], &["loudness", "x.wav", "--frobnicate", "1"], &["loudness", "--target"], &["loudness", "missing.wav"], &["mix"]] {
        let o = run(bad);
        assert_eq!(o.status.code(), Some(2), "{bad:?}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("flux-analyze: "), "{bad:?}: {}", stderr(&o));
        assert!(o.stdout.is_empty(), "{bad:?}");
    }
    assert!(stderr(&run(&["loudness", "x.wav", "--frobnicate", "1"])).contains("unknown option --frobnicate"));
    assert!(stderr(&run(&["loudness", "x.wav", "--target", "loud"])).contains("--target \"loud\""));
}

#[test]
fn loudness_prints_json_or_a_table_and_fails_on_errors() {
    let dir = tempfile::tempdir().unwrap();
    common::write_game(dir.path());
    let o = run(&["loudness", p(dir.path())]);
    // the game has errors (and an unreadable file): exit code 1, the report still printed
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["summary"]["tracks"], 6);
    assert_eq!(v["unreadable"].as_array().unwrap().len(), 1);
    let town = v["tracks"].as_array().unwrap().iter().find(|t| t["name"] == "town.wav").unwrap();
    assert_eq!(town["flags"][0]["code"], "loudness_target");

    let o = run(&["loudness", p(&dir.path().join("explore_loop.wav")), p(&dir.path().join("boss.layers.json")), "--text"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.starts_with("track"), "{text}");
    assert!(text.contains("boss.wav") && text.contains("explore_loop.wav") && text.contains("-15.0"), "{text}");

    // with the target at -20 LUFS the quiet town passes
    let o = run(&["loudness", p(&dir.path().join("town.wav")), "--target", "-20", "--no-loops", "-j", "1"]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
}

#[test]
fn mix_reference_stems_profile_and_spectro_on_files() {
    let dir = tempfile::tempdir().unwrap();
    common::write_game(dir.path());
    let man = dir.path().join("boss.layers.json");
    let master = dir.path().join("boss.wav");

    let o = run(&["mix", p(&master), "--manifest", p(&man)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    // the manifest's sections are used
    assert_eq!(v["tone"].as_array().unwrap().len(), 2);
    assert_eq!(v["sections"][1]["name"], "B");
    let o = run(&["mix", p(&master), "--manifest", p(&man), "--text"]);
    assert!(stdout(&o).contains("mono fold-down"), "{}", stdout(&o));

    let o = run(&["reference", p(&master), p(&dir.path().join("explore_loop.wav")), "--manifest", p(&man), "--sections", p(&man)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(v["whole"]["suggestions"].is_array());
    assert_eq!(v["sections"][0]["against"], "whole");

    let o = run(&["stems", "--manifest", p(&man), "--text"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let t = stdout(&o);
    assert!(t.lines().nth(1).unwrap().starts_with("drums"), "{t}");

    let sections = dir.path().join("sections.json");
    std::fs::write(&sections, r#"[{"name":"first","start_s":0.0,"end_s":1.0},{"name":"rest","start_s":1.0,"end_s":4.0}]"#).unwrap();
    let o = run(&["profile", p(&master), "--sections", p(&sections)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let names: Vec<&str> = v.as_array().unwrap().iter().map(|x| x[0].as_str().unwrap()).collect();
    assert_eq!(names, ["all", "first", "rest"]);

    let png = dir.path().join("spec.png");
    let o = run(&["spectro", p(&png), "0", "4", p(&master), p(&dir.path().join("boss_pads.wav")), "--width", "80", "--height", "40"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(std::fs::read(&png).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));

    // sections outside the audio are refused
    std::fs::write(&sections, r#"[{"name":"late","start_s":10.0,"end_s":12.0}]"#).unwrap();
    for cmd in ["profile", "mix"] {
        let o = run(&[cmd, p(&master), "--sections", p(&sections)]);
        assert_eq!(o.status.code(), Some(2), "{cmd}");
        assert!(stderr(&o).contains("outside"), "{cmd}: {}", stderr(&o));
    }
}
