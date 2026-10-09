//! The analyses on a render in memory (`flux_engine::RenderOutput`), without touching the disk.

mod common;

use flux_analysis::loudness::{GameInput, GameOptions, check_game_audio};
use flux_analysis::meta::Section;
use flux_analysis::mix::{MixOptions, check_mix};
use flux_analysis::profile::profile;
use flux_analysis::reference::{ReferenceOptions, compare};
use flux_analysis::spectro::{SpectroOptions, spectrogram};
use flux_analysis::stems::stem_balance;
use flux_analysis::{AudioRef, Audio};
use flux_engine::RenderOutput;
use flux_engine::render::SectionBound;
use common::{SR, gain_to, plucks, scaled, sine};

/// A two-bar looped render at 120 BPM: plucked notes on the "drums" layer, a steady fifth on "pads", the master
/// their sum at -15 LUFS, panned slightly so the channels differ.
fn render() -> RenderOutput {
    let p = plucks(4.0, 220.0);
    let pads = sine(4.0, 330.0, 0.1);
    let sum: Vec<f32> = p.iter().zip(&pads).map(|(a, b)| a + b).collect();
    let g = gain_to(&sum, -15.0);
    let (drums, pads) = (scaled(&p, g), scaled(&pads, g));
    let (dl, dr) = (scaled(&drums, 1.05), scaled(&drums, 0.95));
    let left: Vec<f32> = dl.iter().zip(&pads).map(|(a, b)| a + b).collect();
    let right: Vec<f32> = dr.iter().zip(&pads).map(|(a, b)| a + b).collect();
    let n = left.len();
    RenderOutput {
        sr: SR,
        left,
        right,
        looped: true,
        loop_samples: n,
        lufs: -15.0,
        true_peak_dbtp: -5.0,
        drive_db: 0.0,
        limiter_gr_db: 0.0,
        seam_error_db: Some(-120.0),
        tracks: vec![],
        sections: vec![
            SectionBound { name: "A".into(), start_bar: 1, bars: 1, start: 0, end: n / 2 },
            SectionBound { name: "B".into(), start_bar: 2, bars: 1, start: n / 2, end: n },
        ],
        render_seconds: 0.1,
        bar_starts: vec![0, n / 2],
        glue_loud_db: None,
        layers: vec![("drums".into(), (dl, dr)), ("pads".into(), (pads.clone(), pads))],
        layer_sum_error_db: Some(-150.0),
        ..Default::default()
    }
}

#[test]
fn a_render_is_checked_as_a_game_track() {
    let out = render();
    let input = GameInput::from_render("capital_city", &out);
    assert_eq!(input.bar_starts, vec![0, 96000]);
    assert_eq!(input.layers.len(), 2);
    let rep = check_game_audio(vec![input], &GameOptions::default()).unwrap();
    let t = &rep.tracks[0];
    assert_eq!(t.name, "capital_city");
    assert!(t.looped && t.path.is_none() && t.bytes.is_none());
    assert!(t.flags.is_empty(), "{:#?}", t.flags);
    assert!((t.stats.integrated_lufs.unwrap() + 15.0).abs() < 0.1, "{:?}", t.stats);
    let seam = t.seam.as_ref().unwrap();
    assert_eq!(seam.reference, Some("bar_lines"));
    assert!(seam.jump_ok);
    let layers = t.layers.as_ref().unwrap();
    assert!(layers.max_error_db.unwrap() < -120.0, "{layers:?}");
    assert_eq!(layers.layers.iter().map(|l| l.layer.as_str()).collect::<Vec<_>>(), ["drums", "pads"]);

    // a layer that no longer matches the master is caught
    let mut stale = render();
    stale.layers.pop();
    let rep = check_game_audio(vec![GameInput::from_render("stale", &stale)], &GameOptions::default()).unwrap();
    assert!(rep.tracks[0].flags.iter().any(|f| f.code == "layer_sum"), "{:#?}", rep.tracks[0].flags);
}

#[test]
fn renders_and_files_mix_in_one_game() {
    let out = render();
    let quiet = Audio::new(SR, scaled(&out.left, 0.5), scaled(&out.right, 0.5)).unwrap();
    let inputs = vec![
        GameInput::from_render("a", &out),
        GameInput::from_render("b", &out),
        GameInput { name: "quiet".into(), audio: quiet.view(), looped: false, bar_starts: vec![], layers: vec![] },
    ];
    let rep = check_game_audio(inputs, &GameOptions::default()).unwrap();
    let q = rep.tracks.iter().find(|t| t.name == "quiet").unwrap();
    let codes: Vec<&str> = q.flags.iter().map(|f| f.code).collect();
    // 6 dB down: off target and 6 LU under the game's median
    assert_eq!(codes, ["loudness_target", "loudness_outlier"], "{:#?}", q.flags);
    assert_eq!(rep.summary.lufs_spread_lu, Some(6.0));
}

#[test]
fn sections_and_stems_of_a_render() {
    let out = render();
    let sections = Section::of_render(&out);
    assert_eq!(sections, [Section::new("A", 0.0, 2.0), Section::new("B", 2.0, 4.0)]);
    let master = AudioRef::master(&out);
    let layers = AudioRef::layers(&out);
    assert_eq!(layers.iter().map(|l| l.0).collect::<Vec<_>>(), ["drums", "pads"]);

    let table = stem_balance(&layers, &sections, None).unwrap();
    assert_eq!(table.sections, ["A", "B"]);
    assert_eq!(table.stems[0].name, "drums", "{table:?}");
    assert!(table.table().starts_with("stem"));

    let mix = check_mix(master, &layers, None, &MixOptions { sections: sections.clone(), ..Default::default() }).unwrap();
    assert_eq!(mix.tone.len(), 2);
    // a note on every beat, the one on B's downbeat counted in B
    assert_eq!(mix.transients.iter().map(|t| t.onsets).collect::<Vec<_>>(), [4, 4], "{:#?}", mix.transients);
    assert!(mix.mono.iter().all(|m| m.flags.is_empty() && m.fold_down_lu.unwrap() > -0.5), "{:#?}", mix.mono);
    // the parts are an octave and a half apart: no fight over a band
    assert!(mix.conflicts.is_empty(), "{:#?}", mix.conflicts);
    let json = serde_json::to_value(&mix).unwrap();
    assert!(json["tone"][0]["harsh_vs_pink_db"].is_number());

    // a mix against itself: nothing to suggest, sections matched by name
    let o = ReferenceOptions { mix_sections: sections.clone(), reference_sections: sections, section_suggestions: 3 };
    let rep = compare(master, master, &layers, &o).unwrap();
    assert!(rep.whole.suggestions.is_empty(), "{:#?}", rep.whole.suggestions);
    assert_eq!(rep.whole.diff.lufs, Some(0.0));
    assert_eq!(rep.sections.iter().map(|s| s.against.as_str()).collect::<Vec<_>>(), ["A", "B"]);
    assert!(rep.sections.iter().all(|s| s.suggestions.is_empty() && s.relative_loudness_lu == Some(0.0)));

    let p = profile(master).unwrap();
    assert!((p.dynamics.lufs.unwrap() + 15.0).abs() < 0.1);
    assert!((p.duration_s - 4.0).abs() < 1e-9);

    let img = spectrogram(&[master, layers[1].1], 0.0, 4.0, &SpectroOptions { width: 64, height: 32, ..Default::default() }).unwrap();
    assert_eq!((img.width, img.height), (64, 64));
    assert!(img.encode_png().unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
}
