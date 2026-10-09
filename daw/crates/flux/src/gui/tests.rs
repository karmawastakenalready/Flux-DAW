//! Headless snapshot tests: the GUI drawn by egui_kittest through wgpu's software adapter (no window, no
//! audio device). Baselines live in `tests/snapshots/` (crate root); `UPDATE_SNAPSHOTS=1 cargo test -p flux`
//! rewrites them after an intended visual change.

use super::*;
use crate::audio::Clip;
use crate::state::{Audition, AuditionMarks, LastRender, Variant};
use flux_core::{Command, CommentOp, Project, Session, commands};
use flux_engine::render::{RenderOutput, SectionBound, TrackStat};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};
use widgets::Ballistic;

const SR: u32 = 44100;

fn cmds(v: Value) -> Vec<Command> {
    serde_json::from_value(v).expect("demo commands")
}

fn notes(spec: impl IntoIterator<Item = (f64, f64, u8, u8)>) -> Value {
    Value::Array(spec.into_iter().map(|(beat, dur, pitch, vel)| json!({"beat": beat, "dur": dur, "pitch": pitch, "vel": vel})).collect())
}

/// A small orchestral loop with sections, six tracks of different kinds and some mixer settings.
pub(super) fn demo_project() -> Project {
    let mut p = Project::new("Sunken Temple", 92.0, 32);
    p.key = "D minor".into();
    let chords: [[u8; 3]; 4] = [[50, 53, 57], [46, 50, 53], [43, 46, 50], [45, 49, 52]];
    let pad = (0..32).flat_map(|bar| chords[bar % 4].map(|n| (bar as f64 * 4.0, 4.0, n + 12, 70)));
    let harp = (4..12).chain(20..28).flat_map(|bar| (0..8).map(move |k| (bar as f64 * 4.0 + k as f64 * 0.5, 0.5, chords[bar % 4][k % 3] + 24 + 12 * (k as u8 / 3 % 2), 64 + (k as u8 * 5) % 30)));
    let low = (4..32).flat_map(|bar| [(bar as f64 * 4.0, 2.0, chords[bar % 4][0] - 12, 80), (bar as f64 * 4.0 + 2.0, 2.0, chords[bar % 4][0] - 5, 72)]);
    let melody = [69u8, 70, 72, 74, 72, 70, 69, 65];
    let flute = (12..28).flat_map(|bar| (0..4).map(move |k| (bar as f64 * 4.0 + k as f64, if k == 3 { 1.0 } else { 0.75 }, melody[(bar * 4 + k) % 8], 84)));
    let taiko = (12..20).flat_map(|bar| [(bar as f64 * 4.0, 0.5, 41u8, 110), (bar as f64 * 4.0 + 1.5, 0.5, 41, 90), (bar as f64 * 4.0 + 3.0, 0.25, 45, 100)]);
    let all = cmds(json!([
        {"op": "set_sections", "sections": [
            {"name": "Intro", "start_bar": 1, "bars": 4}, {"name": "Theme A", "start_bar": 5, "bars": 8},
            {"name": "Theme B", "start_bar": 13, "bars": 8}, {"name": "Theme A'", "start_bar": 21, "bars": 8},
            {"name": "Outro", "start_bar": 29, "bars": 4}]},
        {"op": "add_track", "name": "Warm Pad", "instrument": {"kind": "sfz", "path": "build/sfz/vsco/pad_strings.sfz"}},
        {"op": "add_track", "name": "Harp", "instrument": {"kind": "sfz", "path": "build/sfz/vsco/harp.sfz"}},
        {"op": "add_track", "name": "Low Strings", "instrument": {"kind": "sfz", "path": "build/sfz/vsco/celli_sus.sfz"}},
        {"op": "add_track", "name": "Flute", "instrument": {"kind": "sfz", "path": "build/sfz/vsco/flute_sus.sfz"}},
        {"op": "add_track", "name": "Taiko", "instrument": {"kind": "vst3", "path": "plugins/Taiko Ensemble.vst3"}},
        {"op": "add_track", "name": "Choir Stem", "instrument": {"kind": "audio", "path": "stems/choir.wav"}},
        {"op": "add_notes", "track": "Warm Pad", "notes": notes(pad)},
        {"op": "add_notes", "track": "Harp", "notes": notes(harp)},
        {"op": "add_notes", "track": "Low Strings", "notes": notes(low)},
        {"op": "add_notes", "track": "Flute", "notes": notes(flute)},
        {"op": "add_notes", "track": "Taiko", "notes": notes(taiko)},
        {"op": "set_mixer", "track": "Warm Pad", "mixer": {"gain_db": -6.5, "reverb_send": 0.45, "hpf_hz": 120, "width": 1.4}},
        {"op": "set_mixer", "track": "Harp", "mixer": {"gain_db": -3.0, "pan": -0.35, "reverb_send": 0.3, "lpf_hz": 6500,
            "comp": {"threshold_db": -6, "relative": true, "ratio": 2.5}}},
        {"op": "set_mixer", "track": "Low Strings", "mixer": {"gain_db": -1.5, "pan": 0.2, "eq": [{"kind": "peak", "hz": 300, "db": -2}]}},
        {"op": "set_mixer", "track": "Flute", "mixer": {"gain_db": 1.0, "pan": 0.25, "reverb_send": 0.25, "delay": {"seconds": 0.33, "feedback": 0.25, "mix": 0.15}}},
        {"op": "set_mixer", "track": "Taiko", "mixer": {"gain_db": -4.0, "solo": false, "comp": {"threshold_db": -10, "ratio": 4}}},
        {"op": "set_mixer", "track": "Choir Stem", "mixer": {"gain_db": -9.0, "mute": true}},
        {"op": "add_insert", "track": "Harp", "path": "plugins/ValhallaSupermassive.vst3", "preset_name": "Gemini Lyra"},
        {"op": "add_insert", "track": "Harp", "path": "plugins/Vinyl.vst3", "position": "pre", "bypass": true},
        {"op": "add_insert", "bus": "reverb", "path": "plugins/ValhallaSupermassive.vst3", "preset_name": "Great Annihilator"},
        {"op": "add_insert", "bus": "master", "path": "plugins/Vinyl.vst3"},
    ]));
    for c in &all {
        commands::apply(&mut p, c).expect("demo command");
    }
    p
}

/// A plausible render of `p`: a chord drone whose level follows the sections, with per-track statistics.
pub(super) fn fake_render(p: &Project, revision: u64, seq: u64) -> LastRender {
    let tm = TimeMap::new(p);
    let sec_end = tm.sec(p.total_beats());
    let n = (sec_end * SR as f64) as usize;
    let level = |bar: u32| match p.section_at(bar).map(|s| s.name.as_str()) {
        Some("Intro") => 0.18,
        Some("Theme B") => 0.55,
        Some("Outro") => 0.12,
        _ => 0.34,
    };
    let (mut left, mut right) = (vec![0f32; n], vec![0f32; n]);
    let mut seed = 0x2545_f491_u32;
    for k in 0..n {
        let t = k as f64 / SR as f64;
        let bar = tm.bar_of(tm.beat(t));
        let beat_frac = tm.beat(t).fract();
        let pulse = 0.55 + 0.45 * (-beat_frac * 5.0).exp();
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let noise = (seed as f64 / u32::MAX as f64 - 0.5) * 0.05;
        let tone = (t * 146.83 * std::f64::consts::TAU).sin() * 0.5 + (t * 220.0 * std::f64::consts::TAU).sin() * 0.3 + (t * 587.3 * std::f64::consts::TAU).sin() * 0.12;
        let x = (tone + noise) * level(bar) * pulse;
        left[k] = x as f32;
        right[k] = (x * 0.92 + noise * 0.1) as f32;
    }
    let bar_starts = (1..=p.bars).map(|b| (tm.sec(tm.bar_beat(b)) * SR as f64) as usize).collect();
    let sections = p
        .sections
        .iter()
        .map(|s| SectionBound {
            name: s.name.clone(),
            start_bar: s.start_bar,
            bars: s.bars,
            start: (tm.sec(tm.bar_beat(s.start_bar)) * SR as f64) as usize,
            end: (tm.sec(tm.bar_beat(s.start_bar + s.bars)) * SR as f64) as usize,
        })
        .collect();
    let profile: [(f64, [f64; 5]); 6] = [
        (-14.2, [-24.0, -21.0, -18.5, -20.5, -26.0]),
        (-9.8, [-120.0, -19.5, -17.0, -18.0, -120.0]),
        (-11.5, [-120.0, -22.0, -19.0, -21.5, -24.0]),
        (-7.9, [-120.0, -120.0, -16.5, -17.5, -120.0]),
        (-3.1, [-120.0, -120.0, -13.0, -120.0, -120.0]),
        (-60.0, [-120.0; 5]),
    ];
    let tracks = p
        .tracks
        .iter()
        .zip(profile)
        .map(|(t, (peak, rms))| TrackStat {
            id: t.id,
            name: t.name.clone(),
            engine: "sfizz".into(),
            audible: !t.mixer.mute,
            peak_dbfs: peak,
            rms_db: rms.iter().copied().fold(-120.0, f64::max),
            section_rms_db: rms.to_vec(),
            warning: (t.name == "Taiko").then(|| "sharp attack 14.2 dB: soften with a slower compressor attack".to_string()),
            layer: "base".into(),
            restricted: None,
            comp_loud_db: None,
            timbre: None,
        })
        .collect();
    let out = RenderOutput {
        sr: SR,
        left,
        right,
        looped: p.looped,
        loop_samples: n,
        lufs: -15.1,
        true_peak_dbtp: -1.2,
        drive_db: 4.5,
        limiter_gr_db: -2.3,
        seam_error_db: Some(-71.0),
        tracks,
        sections,
        render_seconds: 6.4,
        bar_starts,
        glue_loud_db: None,
        layers: vec![],
        layer_sum_error_db: None,
        ..Default::default()
    };
    LastRender { out, project: p.clone(), revision, seq }
}

fn clip(key: &str, seconds: f64) -> Arc<Clip> {
    let n = (seconds * SR as f64) as usize;
    Arc::new(Clip { sr: SR, left: vec![0.0; n], right: vec![0.0; n], key: key.into() })
}

fn variant(id: u64, i: usize, name: &str, commands: Vec<Value>, lufs: f64) -> Variant {
    Variant {
        name: name.into(),
        commands,
        clip: clip(&format!("audition {id} / {}", crate::state::letter(i)), 10.4),
        path: std::path::PathBuf::from(format!("build/auditions/sunken_temple/{id:03}_{}.wav", crate::state::letter(i))),
        lufs,
    }
}

/// The demo session behind every snapshot: a rendered project, two auditions and a comment thread.
pub(super) fn demo_shared() -> Arc<Shared> {
    let mut s = Session::new(demo_project());
    s.note(Source::System, "opened daw/projects/sunken_temple.flux.json".into());
    let edits = cmds(json!([{"op": "set_mixer", "track": "Harp", "mixer": {"gain_db": -4.5}}]));
    s.apply_batch(&edits, Source::Agent).unwrap();
    let edits = cmds(json!([{"op": "set_mixer", "track": "Flute", "mixer": {"reverb_send": 0.3}}]));
    s.apply_batch(&edits, Source::Human).unwrap();
    for op in [
        json!({"action": "add", "bar": 9, "to_bar": 12, "track": "Harp", "text": "The harp feels too bright against the pad here."}),
        json!({"action": "add", "bar": 17, "text": "Can the taiko hits land a little softer in Theme B?"}),
        json!({"action": "add", "bar": 29, "text": "Outro could fade the low strings earlier."}),
    ] {
        let op: CommentOp = serde_json::from_value(op).unwrap();
        s.comment(&op, Source::Human).unwrap();
    }
    s.comment(&CommentOp::Reply { id: 1, text: "Lowered the harp LPF to 6.5 kHz and its level by 1.5 dB; listen again?".into() }, Source::Agent).unwrap();
    s.comment(&CommentOp::Resolve { id: 3, reply: Some("Low strings now fade from bar 29.".into()) }, Source::Agent).unwrap();
    let rev = s.revision;
    let project = s.project.clone();
    let sh = Shared::new(s);
    {
        let mut st = sh.lock();
        st.render_seq = 1;
        st.render = Some(Arc::new(fake_render(&project, rev, 1)));
        st.agent = Some("Claude Code".into());
        st.auditions.push(Audition {
            id: 1,
            label: "Pad register".into(),
            question: "Which pad voicing sits better under the theme?".into(),
            from_bar: 5,
            to_bar: 8,
            revision: rev - 1,
            variants: vec![
                variant(1, 0, "current", vec![], -17.2),
                variant(1, 1, "an octave lower", vec![json!({"op": "transpose_notes", "track": "Warm Pad", "semitones": -12})], -16.4),
            ],
            pick: Some(1),
            note: "Lower one is warmer.".into(),
            applied: true,
        });
        st.audition_marks.insert(1, AuditionMarks { picked_by: Some(Source::Human), applied_by: Some(Source::Agent), applied_variant: Some(1) });
        st.auditions.push(Audition {
            id: 2,
            label: "Harp brightness".into(),
            question: "Listen to the harp against the pad: which is least harsh?".into(),
            from_bar: 9,
            to_bar: 12,
            revision: rev,
            variants: vec![
                variant(2, 0, "current", vec![], -18.1),
                variant(2, 1, "LPF 4.5 kHz", vec![json!({"op": "set_mixer", "track": "Harp", "mixer": {"lpf_hz": 4500}})], -18.6),
                variant(2, 2, "LPF 4.5 kHz, softer", vec![json!({"op": "set_mixer", "track": "Harp", "mixer": {"lpf_hz": 4500}}), json!({"op": "scale_velocity", "track": "Harp", "factor": 0.85})], -19.3),
            ],
            pick: None,
            note: String::new(),
            applied: false,
        });
        st.audition_rev = 3;
    }
    sh
}


fn track_id(p: &Project, name: &str) -> u64 {
    p.tracks.iter().find(|t| t.name == name).map(|t| t.id).expect("demo track")
}

/// Every track's live engine state as the player reports it (the VST3 track runs frozen).
fn engine_status(p: &Project) -> Vec<(u64, String)> {
    p.tracks.iter().map(|t| (t.id, if t.name == "Taiko" { "frozen".to_string() } else { "live".to_string() })).collect()
}

/// The player at rest at `bar`, set to play from `mode`.
fn stopped_feed(p: &Project, bar: u32, mode: Mode) -> Feed {
    let tm = TimeMap::new(p);
    Feed {
        playing: false,
        mode,
        song_mode: mode,
        seconds: tm.sec(tm.bar_beat(bar)),
        out: [(f64::NEG_INFINITY, f64::NEG_INFINITY); 2],
        tracks: vec![],
        status: engine_status(p),
        loading: 0,
        clip: None,
    }
}

/// The song playing through the live engine, `beat` beats into `bar`, with output and track levels.
fn live_feed(p: &Project, bar: u32, beat: f64) -> Feed {
    let tm = TimeMap::new(p);
    let levels = [("Warm Pad", 0.16), ("Harp", 0.31), ("Low Strings", 0.42), ("Flute", 0.55), ("Taiko", 0.78)];
    Feed {
        playing: true,
        mode: Mode::Live,
        song_mode: Mode::Live,
        seconds: tm.sec(tm.bar_beat(bar) + beat),
        out: [(-16.2, -7.4), (-16.9, -8.3)],
        tracks: levels.iter().map(|&(n, l)| (track_id(p, n), l)).collect(),
        status: engine_status(p),
        loading: 0,
        clip: None,
    }
}

/// An audition clip playing, `progress` (0..1) through.
fn clip_feed(p: &Project, key: &str, progress: f64) -> Feed {
    Feed {
        playing: true,
        mode: Mode::Clip,
        song_mode: Mode::Live,
        seconds: 4.3,
        out: [(-18.4, -10.6), (-18.9, -11.2)],
        tracks: vec![],
        status: engine_status(p),
        loading: 0,
        clip: Some((key.into(), progress)),
    }
}

/// The part of the app a harness draws.
#[derive(Clone, Copy)]
enum Part {
    /// The whole window.
    Window,
    /// Menu bar and toolbar.
    TopBars,
    /// The right-hand panel (Activity, Comments, Auditions).
    Side,
    /// The bottom panel (Mixer, Track, Master, Analysis).
    Bottom,
    /// Whatever dialog is open, over an empty window.
    Dialogs,
}

/// Draw `part` over the whole screen (kittest hands out a ui inset by a margin), then carry out what was clicked.
fn draw(ui: &mut egui::Ui, g: &mut Gui, part: Part) {
    let r = ui.ctx().content_rect();
    let mut ui = ui.new_child(UiBuilder::new().max_rect(r));
    ui.set_clip_rect(r);
    if let Part::Window = part {
        g.frame(&mut ui);
        return;
    }
    ui.painter().rect_filled(r, 0.0, if let Part::Dialogs = part { LANE } else { PANEL });
    let f = g.read_feed();
    g.meters.update(&f);
    let ctx = ui.ctx().clone();
    let mut act = vec![];
    let sh = g.sh.clone();
    {
        let st = sh.lock();
        match part {
            Part::TopBars => g.top_bars(&mut ui, &st, &f, &mut act),
            Part::Side => g.side_panel(&mut ui, &st, &f, &mut act),
            Part::Bottom => g.bottom(&mut ui, &st, &f, &mut act),
            Part::Dialogs => g.dialogs(&ctx, &st, &mut act),
            Part::Window => unreachable!(),
        }
    }
    g.run_actions(act);
}

/// A harness, holding the GPU for as long as it lives: the software adapter can crash when several tests
/// create devices at once.
struct Stage {
    harness: Harness<'static, Gui>,
    _gpu: std::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for Stage {
    type Target = Harness<'static, Gui>;
    fn deref(&self) -> &Self::Target {
        &self.harness
    }
}

impl std::ops::DerefMut for Stage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.harness
    }
}

static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A harness at 1 point per pixel with the app's theme, drawing `part` of `gui`.
fn stage(w: f32, h: f32, gui: Gui, part: Part) -> Stage {
    // a test that failed while holding the lock leaves nothing behind that matters
    let gpu = GPU.lock().unwrap_or_else(|e| e.into_inner());
    let mut themed = false;
    let app = move |ui: &mut egui::Ui, g: &mut Gui| {
        // the harness runs a first frame while it is built: fonts set now are bound from the next one
        if !themed {
            theme::apply(ui.ctx());
            themed = true;
            ui.ctx().request_repaint();
            return;
        }
        draw(ui, g, part);
    };
    let mut harness = Harness::builder()
        .with_size(egui::vec2(w, h))
        .with_pixels_per_point(1.0)
        // short steps, so the 16-33 ms repaints of meters and playheads are not taken for immediate ones
        .with_step_dt(1.0 / 240.0)
        .with_max_steps(120)
        .wgpu()
        .build_ui_state(app, gui);
    harness.run();
    Stage { harness, _gpu: gpu }
}

/// Take the pointer off the window (no hover state, no cursor in the picture) and let the ui settle.
fn settle(h: &mut Stage) {
    h.event(egui::Event::PointerGone);
    h.run();
}

/// The demo session in a GUI whose player is replaced by `feed`.
fn demo_gui(feed: impl FnOnce(&Project) -> Feed) -> Gui {
    let mut g = Gui::new(demo_shared());
    let f = feed(&g.sh.lock().session.project);
    g.feed = Some(f);
    g
}

fn project_of(g: &Gui) -> Project {
    g.sh.lock().session.project.clone()
}

#[test]
fn window_playing_live() {
    let mut g = demo_gui(|p| live_feed(p, 14, 1.5));
    g.selected = Some(track_id(&project_of(&g), "Harp"));
    let mut h = stage(1280.0, 800.0, g, Part::Window);
    h.snapshot("window_live_mixer");
}

#[test]
fn window_roll_master_comments() {
    let mut g = demo_gui(|p| stopped_feed(p, 9, Mode::Render));
    let p = project_of(&g);
    g.view = View::Roll;
    g.tab = Tab::Master;
    g.side = Side::Comments;
    g.selected = Some(track_id(&p, "Flute"));
    g.comments.focus = Some(1);
    let mut h = stage(1280.0, 800.0, g, Part::Window);
    h.snapshot("window_roll_master_comments");
}

#[test]
fn window_without_tracks() {
    let p = Project::new("Untitled", 120.0, 16);
    let mut g = Gui::new(Shared::new(Session::new(p.clone())));
    g.feed = Some(stopped_feed(&p, 1, Mode::Live));
    let mut h = stage(1280.0, 720.0, g, Part::Window);
    h.snapshot("window_empty");
}

#[test]
fn top_bars() {
    let mut results = egui_kittest::SnapshotResults::new();
    let mut h = stage(1440.0, 70.0, demo_gui(|p| live_feed(p, 14, 1.5)), Part::TopBars);
    results.add(h.try_snapshot("top_bars_live"));
    drop(h);
    // a narrow window playing the render, which the agent has since made out of date
    let g = demo_gui(|p| {
        let mut f = live_feed(p, 22, 0.0);
        f.mode = Mode::Render;
        f.song_mode = Mode::Render;
        f.tracks.clear();
        f
    });
    g.sh.apply(&cmds(json!([{"op": "set_mixer", "track": "Flute", "mixer": {"gain_db": 0.0}}])), Source::Agent).unwrap();
    let mut h = stage(1100.0, 70.0, g, Part::TopBars);
    results.add(h.try_snapshot("top_bars_compact_stale"));
    results.unwrap();
}

#[test]
fn transport_menu() {
    let mut h = stage(760.0, 250.0, demo_gui(|p| stopped_feed(p, 9, Mode::Live)), Part::TopBars);
    h.get_by_label("Transport").click();
    h.run();
    h.hover_at(egui::pos2(260.0, 70.0));
    h.run();
    h.snapshot("menu_transport");
}

#[test]
fn bottom_tabs() {
    let mut results = egui_kittest::SnapshotResults::new();
    // 1230 px is the default window's bottom panel (1560 px less the side panel): every section shows
    for (name, tab, w) in [
        ("bottom_track", Tab::Track, 1280.0),
        ("bottom_track_default", Tab::Track, 1230.0),
        ("bottom_analysis", Tab::Analysis, 1280.0),
        ("bottom_master", Tab::Master, 1230.0),
    ] {
        let mut g = demo_gui(|p| stopped_feed(p, 13, Mode::Live));
        g.tab = tab;
        g.selected = Some(track_id(&project_of(&g), "Harp"));
        let mut h = stage(w, 340.0, g, Part::Bottom);
        results.add(h.try_snapshot(name));
    }
    results.unwrap();
}

/// A cut's slope chip, under its knob, opens a menu; a pick sets the slope. The low cut is off, so its chip is inert.
#[test]
fn slope_chip_sets_the_cut_slope() {
    let mut g = demo_gui(|p| stopped_feed(p, 13, Mode::Live));
    g.tab = Tab::Track;
    let harp = track_id(&project_of(&g), "Harp");
    g.selected = Some(harp);
    let mut h = stage(1230.0, 340.0, g, Part::Bottom);
    h.get_by_label("low cut slope, 12 dB per octave").click();
    h.run();
    assert!(h.query_by_label("24 dB per octave").is_none(), "the low cut is off: no menu");
    h.get_by_label("high cut slope, 12 dB per octave").click();
    h.run();
    h.get_by_label("24 dB per octave").click();
    h.run();
    let st = h.state().sh.lock();
    let t = st.session.project.tracks.iter().find(|t| t.id == harp).unwrap();
    assert_eq!((t.mixer.hpf_slope, t.mixer.lpf_slope), (12, 24));
    assert_eq!(st.session.log.last().unwrap().source, Source::Human);
}

#[test]
fn auditions_tab_while_a_variant_plays() {
    let mut g = demo_gui(|p| clip_feed(p, "audition 2 / B", 0.42));
    g.side = Side::Auditions;
    g.auditions.focus = Some(2);
    let mut h = stage(340.0, 640.0, g, Part::Side);
    h.snapshot("side_auditions_playing");
}

/// Picking, sending a note and applying from the Auditions tab go through `Shared::pick` and
/// `apply_audition` as the human, like the agent's audition tool.
#[test]
fn audition_pick_note_and_apply() {
    let mut g = demo_gui(|p| stopped_feed(p, 9, Mode::Live));
    g.side = Side::Auditions;
    let mut h = stage(340.0, 640.0, g, Part::Side);
    let picks: Vec<_> = h.get_all_by_label("Pick").collect();
    assert_eq!(picks.len(), 3, "audition 2 offers A, B and C (audition 1 is applied)");
    picks[1].click();
    h.run();
    {
        let st = h.state().sh.lock();
        let a = st.auditions.iter().find(|a| a.id == 2).unwrap();
        assert_eq!(a.pick, Some(1));
        assert_eq!(st.audition_marks[&2].picked_by, Some(Source::Human));
        let last = st.session.log.last().unwrap();
        assert_eq!(last.source, Source::Human);
        assert!(last.summary.contains("picked B"), "{}", last.summary);
    }
    h.get_by_role(egui::accesskit::Role::TextInput).focus();
    h.run();
    h.get_by_role(egui::accesskit::Role::TextInput).type_text("B keeps the shimmer without the bite.");
    h.run();
    h.get_by_label("Send note").click();
    h.run();
    assert_eq!(h.state().sh.lock().auditions[1].note, "B keeps the shimmer without the bite.");
    settle(&mut h);
    h.snapshot("side_auditions_picked");

    let rev = h.state().sh.lock().session.revision;
    h.get_by_label("Apply B").click();
    h.run();
    let st = h.state().sh.lock();
    assert!(st.auditions[1].applied);
    assert_eq!(st.audition_marks[&2].applied_by, Some(Source::Human));
    assert_eq!(st.audition_marks[&2].applied_variant, Some(1));
    assert_eq!(st.session.revision, rev + 1, "applying is one undo step");
    let harp = st.session.project.tracks.iter().find(|t| t.name == "Harp").unwrap();
    assert_eq!(harp.mixer.lpf_hz, 4500.0);
}

#[test]
fn comments_tab_list() {
    let mut g = demo_gui(|p| stopped_feed(p, 9, Mode::Live));
    g.side = Side::Comments;
    g.comments.show_resolved = true;
    g.comments.focus = Some(1);
    let mut h = stage(340.0, 640.0, g, Part::Side);
    h.snapshot("side_comments");
}

/// A comment written in the GUI lands in the session as the human's, anchored at the playhead's bar.
#[test]
fn comment_from_the_comments_tab() {
    let mut g = demo_gui(|p| stopped_feed(p, 17, Mode::Live));
    g.side = Side::Comments;
    let mut h = stage(340.0, 640.0, g, Part::Side);
    h.get_by_label("Comment").click();
    h.run();
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text("Let the flute breathe before the theme returns.");
    h.run();
    settle(&mut h);
    h.snapshot("side_comments_composer");
    let before = h.state().sh.lock().session.project.comments.len();
    h.get_by_label("Add comment").click();
    h.run();
    let st = h.state().sh.lock();
    let p = &st.session.project;
    assert_eq!(p.comments.len(), before + 1);
    let c = p.comments.iter().max_by_key(|c| c.id).unwrap();
    assert_eq!(c.author, Source::Human);
    assert_eq!(c.bar, 17);
    assert_eq!(c.text, "Let the flute breathe before the theme returns.");
    assert_eq!(h.state().comments.focus, Some(c.id));
}

/// A fixed "now" for the Open dialog's relative times (2026-10-08).
fn open_now() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_791_460_800)
}

fn listed_projects() -> Vec<(std::path::PathBuf, Option<std::time::SystemTime>)> {
    let now = open_now();
    let ago = |s: u64| Some(now - std::time::Duration::from_secs(s));
    let file = |name: &str| flux_engine::paths::resolve(&format!("daw/projects/{name}.flux.json"));
    vec![
        (file("sunken_temple"), ago(4 * 60)),
        (file("glass_garden_battle"), ago(3 * 3600)),
        (file("ember_forge"), ago(30 * 3600)),
        (file("tide_caves_explore"), ago(5 * 86400)),
        (file("skyward_spire"), ago(40 * 86400)),
    ]
}

/// The Open dialog lists projects newest first and, with unsaved edits, asks before replacing them.
#[test]
fn open_dialog_asks_to_save_first() {
    let g = demo_gui(|p| stopped_feed(p, 1, Mode::Live));
    g.sh.lock().session.path = Some(flux_engine::paths::resolve("daw/projects/sunken_temple.flux.json"));
    assert!(g.sh.lock().session.is_dirty());
    let mut g = g;
    g.open = Some(dialogs::OpenDialog::with_files(listed_projects(), open_now(), Some(1), None));
    let mut results = egui_kittest::SnapshotResults::new();
    let mut h = stage(720.0, 560.0, g, Part::Dialogs);
    results.add(h.try_snapshot("dialog_open"));
    h.get_by_label("Open").click();
    h.run();
    settle(&mut h);
    results.add(h.try_snapshot("dialog_open_confirm"));
    // that file doesn't exist: the dialog says so and the project stays
    h.get_by_label("Don't save").click();
    h.run();
    assert!(h.state().open.is_some(), "a failed open keeps the dialog");
    assert!(h.query_by_label_contains("glass_garden_battle").is_some(), "the error names the file");
    assert_eq!(h.state().sh.lock().session.project.name, "Sunken Temple");
    results.unwrap();
}

/// Opening a real file from the dialog replaces the project as the human and leaves it unmodified.
#[test]
fn open_dialog_opens_a_typed_path() {
    let dir = std::env::temp_dir().join(format!("flux-gui-test-{}", std::process::id()));
    let path = dir.join("glass_garden.flux.json");
    Session::new(Project::new("Glass Garden", 128.0, 8)).save(Some(&path)).unwrap();
    let mut g = demo_gui(|p| stopped_feed(p, 1, Mode::Live));
    g.open = Some(dialogs::OpenDialog::with_files(vec![], open_now(), None, None));
    let mut h = stage(720.0, 560.0, g, Part::Dialogs);
    h.get_by_role(egui::accesskit::Role::TextInput).focus();
    h.run();
    h.get_by_role(egui::accesskit::Role::TextInput).type_text(&path.to_string_lossy());
    h.run();
    h.get_by_label("Open").click();
    h.run();
    h.get_by_label("Don't save").click();
    h.run();
    {
        let st = h.state().sh.lock();
        assert_eq!(st.session.project.name, "Glass Garden");
        assert_eq!(st.session.path.as_deref(), Some(path.as_path()));
        assert!(!st.session.is_dirty(), "a project just opened has nothing unsaved");
        assert_eq!(st.session.log.last().map(|e| e.source), Some(Source::Human));
    }
    assert!(h.state().open.is_none(), "the dialog closes once the project is open");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn about_dialog() {
    let mut g = demo_gui(|p| stopped_feed(p, 1, Mode::Live));
    g.about = true;
    let mut h = stage(560.0, 440.0, g, Part::Dialogs);
    h.snapshot("dialog_about");
}

#[test]
fn relative_file_times() {
    let now = open_now();
    let ago = |s: u64| dialogs::ago(now, now - std::time::Duration::from_secs(s));
    assert_eq!(ago(20), "Just now");
    assert_eq!(ago(4 * 60), "4 min ago");
    assert_eq!(ago(3600 + 5), "1 hour ago");
    assert_eq!(ago(5 * 3600), "5 hours ago");
    assert_eq!(ago(30 * 3600), "Yesterday");
    assert_eq!(ago(3 * 86400), "3 days ago");
    assert_eq!(ago(40 * 86400), "29 Aug 2026");
}

/// Meters jump up at once, hold the peak for a moment, then fall at a steady rate.
#[test]
fn meter_ballistics() {
    let mut b = Ballistic::default();
    assert!(b.idle());
    b.update(-12.0, -6.0, 0.0);
    assert_eq!((b.level, b.hold), (-12.0, -6.0));
    b.update(f64::NEG_INFINITY, f64::NEG_INFINITY, 0.5);
    assert!((b.level - -22.0).abs() < 1e-9, "falls 20 dB/s: {}", b.level);
    assert_eq!(b.hold, -6.0, "the peak holds");
    for _ in 0..40 {
        b.update(f64::NEG_INFINITY, f64::NEG_INFINITY, 0.25);
    }
    assert!(b.idle(), "silence settles the meter, so the GUI can stop repainting");
}

/// The meters read the feed: track levels arrive as linear peaks, the output as (RMS, peak) dBFS.
#[test]
fn meters_follow_the_feed() {
    let p = demo_project();
    let mut m = Meters::default();
    m.update(&live_feed(&p, 5, 0.0));
    assert!(m.active());
    let taiko = track_id(&p, "Taiko");
    assert!((m.track(taiko).level - 20.0 * 0.78f64.log10()).abs() < 1e-3);
    assert_eq!(m.out[0].hold, -7.4);
    m.reset_peaks();
    assert_eq!(m.out[0].hold, m.out[0].level);
    assert!(!m.clipped);
}

/// The sentence under an audition says who chose and who applied, and stays out of the way otherwise.
#[test]
fn audition_attribution() {
    let sh = demo_shared();
    let st = sh.lock();
    let a = st.auditions.iter().find(|a| a.id == 1).unwrap();
    let marks = |picked_by, applied_by, applied_variant| AuditionMarks { picked_by, applied_by, applied_variant };
    assert_eq!(side::attribution(a, &st.audition_marks[&1]).as_deref(), Some("Picked by you, applied by the agent."));
    assert_eq!(side::attribution(a, &marks(None, None, None)), None, "the Applied tag says it all");
    assert_eq!(side::attribution(a, &marks(Some(Source::Agent), Some(Source::Human), Some(0))).as_deref(), Some("Picked by the agent, variant A applied by you instead."));
    assert_eq!(side::attribution(a, &marks(None, Some(Source::Human), Some(0))).as_deref(), Some("Variant A applied by you instead."));
    let open = st.auditions.iter().find(|a| a.id == 2).unwrap();
    assert_eq!(side::attribution(open, &marks(None, None, None)), None, "nothing picked yet");
}

/// The window icon and every in-app mark decode, at the size their name says (a broken file would only be
/// logged at startup, so catch it here).
#[test]
fn bundled_marks_decode() {
    for (size, bytes) in MARKS {
        let icon = eframe::icon_data::from_png_bytes(bytes).unwrap_or_else(|e| panic!("flux-{size}.png: {e}"));
        assert_eq!((icon.width, icon.height), (size, size), "flux-{size}.png");
    }
}
