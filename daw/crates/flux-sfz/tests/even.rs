//! `even` through the real sfizz library: two keys 12 dB apart end up at the same level.
//!
//! Skipped (with a message) when the sfizz library cannot be loaded on this machine.

use flux_sfz::Instrument;
use flux_sfz::edit::write_files;
use flux_sfz::even::{EvenOptions, even};
use std::path::Path;

/// A one-second mono 16-bit sine.
fn sine_wav(path: &Path, hz: f64, amplitude: f64) {
    let sr = 44_100u32;
    let mut data = Vec::with_capacity(sr as usize * 2);
    for i in 0..sr {
        let v = amplitude * (2.0 * std::f64::consts::PI * hz * f64::from(i) / f64::from(sr)).sin();
        data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
    }
    let mut b = Vec::with_capacity(44 + data.len());
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&sr.to_le_bytes());
    b.extend_from_slice(&(sr * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&data);
    std::fs::write(path, b).unwrap();
}

#[test]
fn keys_are_brought_to_the_median() {
    if let Err(e) = flux_engine::sfizz::api() {
        eprintln!("skipped: sfizz is not available ({e})");
        return;
    }
    let d = std::env::temp_dir().join(format!("flux-sfz-even-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("lib")).unwrap();
    sine_wav(&d.join("lib/loud.wav"), 261.63, 0.5);
    sine_wav(&d.join("lib/soft.wav"), 293.66, 0.125);
    std::fs::write(
        d.join("lib/map.sfz"),
        "<group> ampeg_release=5 amp_veltrack=0\n\
         <region> sample=loud.wav key=60\n\
         <region> sample=soft.wav key=62 volume=-1\n",
    )
    .unwrap();
    let inst = Instrument::load(d.join("lib/map.sfz")).unwrap();
    let options = EvenOptions { velocities: vec![100], release_s: Some(0.3), ..EvenOptions::default() };
    let (files, report) = even(&inst, &d.join("out/even.sfz"), &options).unwrap();
    // 0.5 and 0.125 (-1 dB) are 13 dB apart: each moves 6.5 dB toward the middle
    let change = |key: u8| report.regions.iter().find(|r| r.key == key).unwrap().change_db;
    assert!((change(60) + 6.5).abs() < 0.1, "{report:?}");
    assert!((change(62) - 6.5).abs() < 0.1, "{report:?}");
    assert_eq!(report.release_opcodes_changed, 1);
    write_files(&files).unwrap();

    // text that is not on disk (a fragment, an edited copy) is played from memory
    let text = std::fs::read_to_string(d.join("lib/map.sfz")).unwrap();
    let virtual_inst = Instrument::from_text(&text, d.join("lib/not_on_disk.sfz"), &Default::default()).unwrap();
    let (_, from_text) = even(&virtual_inst, &d.join("out/virtual.sfz"), &options).unwrap();
    assert_eq!(from_text.targets_db, report.targets_db);

    // measured again, the copy is even
    let evened = Instrument::load(d.join("out/even.sfz")).unwrap();
    assert_eq!(evened.playable_regions().count(), 2);
    let (_, again) = even(&evened, &d.join("out/again.sfz"), &options).unwrap();
    for k in &again.keys {
        assert!((k.levels_db[0] - again.targets_db[0]).abs() < 0.1, "{again:?}");
    }
    let _ = std::fs::remove_dir_all(&d);
}
