//! Editors for the built-in effects of a channel strip and of the master (EQ, compressor, dynamic EQ, de-esser,
//! reverb, multiband, limiter) and for a track's routing. Each shows the current settings and returns new ones when
//! a gesture ends; the panels turn them into commands, so every edit is one undo step in the activity log, as the
//! agent's are.

use super::menu_item;
use super::theme::*;
use super::widgets::*;
use eframe::egui::{self, Align2, Id, Popup, Pos2, Response, RichText, Sense, Ui, vec2};
use flux_core::commands::{MixerPatch, SendSpec, reaches};
use flux_core::{Comp, DeEss, Detector, DynBand, EqBand, EqKind, Instrument, Limiter, MbBand, Multiband, Project, ReverbAlgorithm, ReverbFx, ReverbParams, Track, TrackRef};

/// Width of a number column in an effect's table.
const COL_W: f32 = 42.0;
/// Width of a band's type menu.
const KIND_W: f32 = 76.0;
/// Width of a multiband band's name.
const BAND_W: f32 = 60.0;
/// Width of a row's remove button.
const DEL_W: f32 = 20.0;
/// Space between the controls of a row.
const GAP: f32 = 4.0;
/// Width of the routing menus.
const MENU_W: f32 = 200.0;

/// Width of an effect's section while it's off (or has no bands): its switch and a line about what it does.
pub const OFF_W: f32 = 168.0;
/// Width of an EQ's band table.
pub const EQ_W: f32 = KIND_W + 3.0 * COL_W + DEL_W + 4.0 * GAP;
/// Width of a dynamic EQ's band table.
pub const DYN_EQ_W: f32 = KIND_W + 8.0 * COL_W + DEL_W + 9.0 * GAP;
/// Width of the multiband compressor's band table.
pub const MULTIBAND_W: f32 = BAND_W + 7.0 * COL_W + 7.0 * GAP;
/// Width of the routing section.
pub const ROUTING_W: f32 = 236.0;

/// Width of a row of `n` knobs, each `kw` wide.
pub fn knobs_w(n: usize, kw: f32) -> f32 {
    n as f32 * kw + n.saturating_sub(1) as f32 * GAP
}

// ---------------------------------------------------------------------------------------------
// formatting

/// A frequency in a cell: "250", "3.15k", "12.5k".
fn cell_hz(v: f64) -> String {
    if v >= 10000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if v >= 1000.0 {
        format!("{:.2}k", v / 1000.0)
    } else {
        format!("{v:.0}")
    }
}
fn cell_db(v: f64) -> String {
    format!("{v:+.1}")
}
fn cell_q(v: f64) -> String {
    format!("{v:.2}")
}
fn cell_ratio(v: f64) -> String {
    format!("{v:.1}")
}
fn cell_ms(v: f64) -> String {
    if v < 10.0 { format!("{v:.1}") } else { format!("{v:.0}") }
}
fn cell_pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}
fn f_knee(v: f64) -> String {
    if v < 0.05 { "hard".into() } else { format!("{v:.1} dB") }
}
fn f_attack(v: f64) -> String {
    if v < 10.0 { format!("{v:.1} ms") } else { f_ms(v) }
}
fn f_lookahead(v: f64) -> String {
    if v <= 0.0 { "off".into() } else { format!("{v:.1} ms") }
}
fn f_decay(v: f64) -> String {
    if v <= 0.0 { "auto".into() } else { format!("{v:.1} s") }
}
fn f_release(v: f64) -> String {
    if v <= 0.0 { "fast".into() } else { f_ms(v) }
}
fn f_clip(v: f64) -> String {
    if v <= 0.0 { "off".into() } else { format!("{v:.1} dB") }
}

/// Names joined for a sentence: "A", "A and B", "A, B and C".
fn and_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

// ---------------------------------------------------------------------------------------------
// building blocks

/// A row of controls, `GAP` apart.
fn row<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        add(ui)
    })
    .inner
}

/// A table's column names, numbers' right-aligned over their values.
fn header(ui: &mut Ui, cols: &[(f32, &str)]) {
    row(ui, |ui| {
        for (i, &(w, name)) in cols.iter().enumerate() {
            let (r, _) = ui.allocate_exact_size(vec2(w, 14.0), Sense::hover());
            let (x, align) = if i == 0 { (r.left() + 1.0, Align2::LEFT_CENTER) } else { (r.right() - 5.0, Align2::RIGHT_CENTER) };
            ui.painter().text(Pos2::new(x, r.center().y), align, name, body(10.0), TEXT3);
        }
    });
}

/// A number cell a table column wide.
fn num(label: &str, min: f64, max: f64, fmt: fn(f64) -> String) -> Cell<'_> {
    Cell::new(label, min, max, fmt).width(COL_W)
}

/// A measured value in a table column, read-only.
fn readout(ui: &mut Ui, text: &str, color: egui::Color32, tip: &str) {
    let (r, resp) = ui.allocate_exact_size(vec2(COL_W, CELL_H), Sense::hover());
    ui.painter().text(Pos2::new(r.right() - 5.0, r.center().y), Align2::RIGHT_CENTER, text, mono(10.5), color);
    resp.on_hover_text(tip);
}

/// A row's remove button.
fn remove(ui: &mut Ui, hint: &str) -> bool {
    icon_button_ex(ui, Icon::Close, false, true, DEL_W, hint).clicked()
}

/// An effect's on/off switch; while it's on, `more` fills the rest of the row. Returns a flip.
fn on_off(ui: &mut Ui, on: bool, more: impl FnOnce(&mut Ui)) -> Option<bool> {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let mut v = on;
        let flipped = switch(ui, &mut v, if on { "On" } else { "Off" });
        if on {
            more(ui);
        }
        flipped.then_some(v)
    })
    .inner
}

/// A row of knobs.
fn knob_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(6.0);
    row(ui, add);
}

/// What a relative threshold counts from: the loud level the last render measured where the effect listens.
fn loud_caption(ui: &mut Ui, relative: bool, loud_db: Option<f64>) {
    ui.add_space(4.0);
    let text = match (relative, loud_db) {
        (false, _) => "Threshold in dBFS".to_string(),
        (true, Some(l)) => format!("Loud level at the last render: {l:.1} dBFS"),
        (true, None) => "Render to measure the loud level".to_string(),
    };
    let label = ui.add(egui::Label::new(RichText::new(text).font(body(11.0)).color(TEXT3)).wrap());
    if relative {
        label.on_hover_text("The threshold counts from the loud level, so the effect acts the same however loud the part is played");
    }
}

// ---------------------------------------------------------------------------------------------
// EQ

/// The kinds an EQ band can be, as its menu lists them.
const EQ_KINDS: [(EqKind, &str, &str); 5] = [
    (EqKind::Peak, "Peak", ""),
    (EqKind::LowShelf, "Low shelf", ""),
    (EqKind::HighShelf, "High shelf", ""),
    (EqKind::Notch, "Notch", "cuts a narrow band"),
    (EqKind::Tilt, "Tilt", "tips the balance"),
];

/// The kinds a dynamic EQ band can be.
const DYN_KINDS: [(EqKind, &str, &str); 3] = [(EqKind::Peak, "Peak", ""), (EqKind::LowShelf, "Low shelf", ""), (EqKind::HighShelf, "High shelf", "")];

fn kind_name(k: EqKind) -> &'static str {
    match k {
        EqKind::Peak => "Peak",
        EqKind::LowShelf => "Low shelf",
        EqKind::HighShelf => "High shelf",
        EqKind::Notch => "Notch",
        EqKind::Tilt => "Tilt",
    }
}

/// A frequency for a new band, away from the bands already there: 1 kHz, or the first of a few spread across.
fn free_hz(used: &[f64]) -> f64 {
    [1000.0, 250.0, 4000.0, 100.0, 8000.0, 500.0, 2000.0].into_iter().find(|&f: &f64| used.iter().all(|u| (u / f).log2().abs() > 0.5)).unwrap_or(1000.0)
}

/// An EQ: a row per band (type, frequency, gain, Q; a notch has no gain and a tilt no Q) and a button adding one.
/// `name` begins the cells' names ("EQ", "Master EQ"). Returns the new band list.
pub fn eq(ui: &mut Ui, key: &str, name: &str, bands: &[EqBand]) -> Option<Vec<EqBand>> {
    let base = Id::new(key);
    let mut out = None;
    if bands.is_empty() {
        caption(ui, "No bands");
    } else {
        header(ui, &[(KIND_W, "Type"), (COL_W, "Hz"), (COL_W, "dB"), (COL_W, "Q")]);
        for (i, b) in bands.iter().enumerate() {
            let id = base.with(i);
            let n = i + 1;
            let set = |nb: EqBand| {
                let mut v = bands.to_vec();
                v[i] = nb;
                Some(v)
            };
            let label = |what: &str| format!("{name} band {n} {what}");
            row(ui, |ui| {
                let kind = kind_name(b.kind);
                let resp = chip(ui, KIND_W, kind, true, &label(&format!("type, {kind}"))).on_hover_text("The band's shape");
                if let Some(k) = menu_pick(&resp, b.kind, &EQ_KINDS, 200.0) {
                    out = set(EqBand { kind: k, ..*b });
                }
                if let Some(v) = num(&label("frequency"), 20.0, 20000.0, cell_hz).scale(Scale::Log).show(ui, id.with("hz"), b.hz) {
                    out = set(EqBand { hz: v, ..*b });
                }
                let gain = label("gain");
                if let Some(v) = num(&gain, -24.0, 24.0, cell_db).enabled(b.kind != EqKind::Notch).show(ui, id.with("db"), b.db) {
                    out = set(EqBand { db: v, ..*b });
                }
                let q = label("Q");
                if let Some(v) = num(&q, 0.1, 10.0, cell_q).scale(Scale::Log).enabled(b.kind != EqKind::Tilt).show(ui, id.with("q"), b.q) {
                    out = set(EqBand { q: v, ..*b });
                }
                if remove(ui, "Remove the band (undo brings it back)") {
                    let mut v = bands.to_vec();
                    v.remove(i);
                    out = Some(v);
                }
            });
        }
    }
    ui.add_space(4.0);
    if button_ex(ui, Some(Icon::Plus), "Band", Kind::Secondary, true).on_hover_text("Add a band: a peak at 0 dB, ready to shape").clicked() {
        let used: Vec<f64> = bands.iter().map(|b| b.hz).collect();
        let mut v = bands.to_vec();
        v.push(EqBand { kind: EqKind::Peak, hz: free_hz(&used), db: 0.0, q: 0.9 });
        out = Some(v);
    }
    out
}

/// A dynamic EQ: a row per band (type, frequency, Q, threshold, range, ratio, attack, release, and the loud level its
/// relative threshold counted from at the last render; `loud` per band, None where unmeasured) and a button adding
/// one. Returns the new band list.
pub fn dyn_eq(ui: &mut Ui, key: &str, bands: &[DynBand], loud: &[Option<f64>]) -> Option<Vec<DynBand>> {
    let base = Id::new(key);
    let mut out = None;
    if bands.is_empty() {
        caption(ui, "No bands");
    } else {
        header(ui, &[(KIND_W, "Type"), (COL_W, "Hz"), (COL_W, "Q"), (COL_W, "Thr dB"), (COL_W, "Range"), (COL_W, "Ratio"), (COL_W, "Att ms"), (COL_W, "Rel ms"), (COL_W, "Loud")]);
        for (i, b) in bands.iter().enumerate() {
            let id = base.with(i);
            let n = i + 1;
            let set = |nb: DynBand| {
                let mut v = bands.to_vec();
                v[i] = nb;
                Some(v)
            };
            let label = |what: &str| format!("Dynamic EQ band {n} {what}");
            row(ui, |ui| {
                let kind = kind_name(b.kind);
                let resp = chip(ui, KIND_W, kind, true, &label(&format!("type, {kind}"))).on_hover_text("What the band turns down: around its frequency, or everything below or above it");
                if let Some(k) = menu_pick(&resp, b.kind, &DYN_KINDS, 160.0) {
                    out = set(DynBand { kind: k, ..*b });
                }
                if let Some(v) = num(&label("frequency"), 20.0, 20000.0, cell_hz).scale(Scale::Log).show(ui, id.with("hz"), b.hz) {
                    out = set(DynBand { hz: v, ..*b });
                }
                if let Some(v) = num(&label("Q"), 0.1, 10.0, cell_q).scale(Scale::Log).show(ui, id.with("q"), b.q) {
                    out = set(DynBand { q: v, ..*b });
                }
                let (lo, hi) = if b.relative { (-30.0, 12.0) } else { (-80.0, 0.0) };
                if let Some(v) = num(&label("threshold"), lo, hi, cell_db).show(ui, id.with("thr"), b.threshold_db) {
                    out = set(DynBand { threshold_db: v, ..*b });
                }
                if let Some(v) = num(&label("range"), -24.0, 0.0, cell_db).show(ui, id.with("range"), b.range_db) {
                    out = set(DynBand { range_db: v, ..*b });
                }
                if let Some(v) = num(&label("ratio"), 1.0, 20.0, cell_ratio).scale(Scale::Log).show(ui, id.with("ratio"), b.ratio) {
                    out = set(DynBand { ratio: v, ..*b });
                }
                if let Some(v) = num(&label("attack"), 0.1, 500.0, cell_ms).scale(Scale::Log).show(ui, id.with("att"), b.attack_ms) {
                    out = set(DynBand { attack_ms: v, ..*b });
                }
                if let Some(v) = num(&label("release"), 5.0, 2000.0, cell_ms).scale(Scale::Log).show(ui, id.with("rel"), b.release_ms) {
                    out = set(DynBand { release_ms: v, ..*b });
                }
                match (b.relative, loud.get(i).copied().flatten()) {
                    (false, _) => readout(ui, "dBFS", TEXT3, "This band's threshold is in dBFS"),
                    (true, Some(l)) => readout(ui, &format!("{l:.1}"), TEXT2, "The band's loud level at the last render (dBFS): its threshold counts from it"),
                    (true, None) => readout(ui, "\u{2013}", TEXT3, "Render to measure the band's loud level, which its threshold counts from"),
                }
                if remove(ui, "Remove the band (undo brings it back)") {
                    let mut v = bands.to_vec();
                    v.remove(i);
                    out = Some(v);
                }
            });
        }
    }
    ui.add_space(4.0);
    if button_ex(ui, Some(Icon::Plus), "Band", Kind::Secondary, true).on_hover_text("Add a band that turns its range down while it's loud").clicked() {
        let used: Vec<f64> = bands.iter().map(|b| b.hz).collect();
        let mut v = bands.to_vec();
        v.push(DynBand { kind: EqKind::Peak, hz: free_hz(&used), q: 2.0, threshold_db: -3.0, relative: true, ratio: 3.0, range_db: -6.0, attack_ms: 5.0, release_ms: 80.0 });
        out = Some(v);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// dynamics

/// A compressor: on/off, its detector and auto release, then its knobs in two rows and the loud level a relative
/// threshold counted from at the last render. `fresh` is what turning it on starts from (and what double-clicking a
/// knob resets to). Returns the new settings (None = off).
pub fn comp(ui: &mut Ui, key: &str, c: Option<Comp>, fresh: Comp, kw: f32, loud_db: Option<f64>) -> Option<Option<Comp>> {
    let mut out = None;
    let flip = on_off(ui, c.is_some(), |ui| {
        let Some(c) = c else { return };
        let mut det = c.detector;
        let modes = [
            (Detector::Peak, "Peak", true, "Follows the peaks: catches transients"),
            (Detector::Rms, "RMS", true, "Follows the loudness: smoother, for buses and long notes"),
        ];
        if segmented(ui, &format!("{key}_detector"), &mut det, &modes) {
            out = Some(Some(Comp { detector: det, ..c }));
        }
        let mut auto = c.auto_release;
        if switch(ui, &mut auto, "Auto release") {
            out = Some(Some(Comp { auto_release: auto, ..c }));
        }
    });
    if let Some(on) = flip {
        out = Some(on.then_some(c.unwrap_or(fresh)));
    }
    let Some(c) = c else { return out };
    let base = Id::new(key);
    let mut k = |ui: &mut Ui, knob: Knob, what: &str, v: f64, make: &dyn Fn(f64) -> Comp| {
        if let Some(x) = knob.width(kw).show(ui, base.with(what), v) {
            out = Some(Some(make(x)));
        }
    };
    let (lo, hi) = if c.relative { (-30.0, 6.0) } else { (-60.0, 0.0) };
    knob_row(ui, |ui| {
        k(ui, Knob::new("Threshold", lo, hi, fresh.threshold_db.clamp(lo, hi), f_db), "thr", c.threshold_db, &|v| Comp { threshold_db: v, ..c });
        k(ui, Knob::new("Ratio", 1.0, 20.0, fresh.ratio, f_ratio).scale(Scale::Log), "ratio", c.ratio, &|v| Comp { ratio: v, ..c });
        k(ui, Knob::new("Attack", 0.1, 200.0, fresh.attack_ms, f_attack).scale(Scale::Log), "att", c.attack_ms, &|v| Comp { attack_ms: v, ..c });
        k(ui, Knob::new("Release", 5.0, 2000.0, fresh.release_ms, f_ms).scale(Scale::Log), "rel", c.release_ms, &|v| Comp { release_ms: v, ..c });
        k(ui, Knob::new("Makeup", 0.0, 24.0, 0.0, f_db), "makeup", c.makeup_db, &|v| Comp { makeup_db: v, ..c });
    });
    knob_row(ui, |ui| {
        k(ui, Knob::new("Knee", 0.0, 24.0, 0.0, f_knee), "knee", c.knee_db, &|v| Comp { knee_db: v, ..c });
        k(ui, Knob::new("SC HPF", 20.0, 1000.0, 0.0, f_hz).scale(Scale::LogOff), "sc", c.sidechain_hpf_hz, &|v| Comp { sidechain_hpf_hz: v, ..c });
        k(ui, Knob::new("Lookahead", 0.0, 20.0, 0.0, f_lookahead), "look", c.lookahead_ms, &|v| Comp { lookahead_ms: v, ..c });
        k(ui, Knob::new("Mix", 0.0, 1.0, 1.0, f_pct), "mix", c.mix, &|v| Comp { mix: v, ..c });
    });
    loud_caption(ui, c.relative, loud_db);
    out
}

/// A de-esser: on/off, then its frequency, threshold and range, and the loud level its threshold counted from at the
/// last render. Returns the new settings (None = off).
pub fn deess(ui: &mut Ui, key: &str, d: Option<DeEss>, kw: f32, loud_db: Option<f64>) -> Option<Option<DeEss>> {
    let mut out = None;
    if let Some(on) = on_off(ui, d.is_some(), |_| {}) {
        out = Some(on.then(|| d.unwrap_or_default()));
    }
    let Some(d) = d else { return out };
    let base = Id::new(key);
    let fresh = DeEss::default();
    knob_row(ui, |ui| {
        if let Some(v) = Knob::new("Frequency", 1000.0, 16000.0, fresh.hz, f_hz).scale(Scale::Log).width(kw).show(ui, base.with("hz"), d.hz) {
            out = Some(Some(DeEss { hz: v, ..d }));
        }
        if let Some(v) = Knob::new("Threshold", -30.0, 12.0, fresh.threshold_db, f_db).width(kw).show(ui, base.with("thr"), d.threshold_db) {
            out = Some(Some(DeEss { threshold_db: v, ..d }));
        }
        if let Some(v) = Knob::new("Range", -24.0, 0.0, fresh.range_db, f_db).width(kw).show(ui, base.with("range"), d.range_db) {
            out = Some(Some(DeEss { range_db: v, ..d }));
        }
    });
    loud_caption(ui, true, loud_db);
    out
}

/// The master's limiter: auto release, then its lookahead, release and the soft-clipping stage. Returns the new
/// settings.
pub fn limiter(ui: &mut Ui, key: &str, l: Limiter, kw: f32) -> Option<Limiter> {
    let mut out = None;
    let mut auto = l.auto_release;
    if switch(ui, &mut auto, "Auto release") {
        out = Some(Limiter { auto_release: auto, ..l });
    }
    let base = Id::new(key);
    let fresh = Limiter::default();
    knob_row(ui, |ui| {
        if let Some(v) = Knob::new("Lookahead", 0.5, 20.0, fresh.lookahead_ms, |v| format!("{v:.1} ms")).width(kw).show(ui, base.with("look"), l.lookahead_ms) {
            out = Some(Limiter { lookahead_ms: v, ..l });
        }
        if let Some(v) = Knob::new("Release", 5.0, 2000.0, fresh.release_ms, f_release).scale(Scale::LogOff).width(kw).show(ui, base.with("rel"), l.release_ms) {
            out = Some(Limiter { release_ms: v, ..l });
        }
        if let Some(v) = Knob::new("Clip", 0.0, 6.0, fresh.clip_db, f_clip).width(kw).show(ui, base.with("clip"), l.clip_db) {
            out = Some(Limiter { clip_db: if v < 0.05 { 0.0 } else { v }, ..l });
        }
    });
    out
}

// ---------------------------------------------------------------------------------------------
// multiband

/// The names of a multiband compressor's bands, low to high.
fn band_names(n: usize) -> &'static [&'static str] {
    match n {
        2 => &["Low", "High"],
        3 => &["Low", "Mid", "High"],
        _ => &["Low", "Low mid", "High mid", "High"],
    }
}

/// What band `i` covers, in words.
fn band_range(mb: &Multiband, i: usize) -> String {
    let x = &mb.crossovers_hz;
    match (i.checked_sub(1).and_then(|j| x.get(j)), x.get(i)) {
        (None, Some(hi)) => format!("Below {}", f_hz(*hi)),
        (Some(lo), Some(hi)) => format!("{} to {}", f_hz(*lo), f_hz(*hi)),
        (Some(lo), None) => format!("Above {}", f_hz(*lo)),
        (None, None) => "The whole range".into(),
    }
}

/// One more band: a crossover above the top one, or below the lowest without room there, or else in the middle of
/// the widest gap; the band it splits off copies the band it came from. False when there is no room anywhere.
fn add_band(m: &mut Multiband) -> bool {
    let x = &m.crossovers_hz;
    let (Some(&first), Some(&last)) = (x.first(), x.last()) else { return false };
    let (at, hz) = if last * 3.0 <= 16000.0 {
        (x.len(), last * 3.0)
    } else if first / 3.0 >= 30.0 {
        (0, first / 3.0)
    } else {
        let Some((k, w)) = x.windows(2).enumerate().max_by(|a, b| (a.1[1] / a.1[0]).total_cmp(&(b.1[1] / b.1[0]))) else { return false };
        if w[1] / w[0] < 2.3 {
            return false;
        }
        (k + 1, (w[0] * w[1]).sqrt())
    };
    let Some(&src) = m.bands.get(at) else { return false };
    m.crossovers_hz.insert(at, hz);
    m.bands.insert(at, src);
    true
}

/// The multiband with `n` bands (2 to 4): bands added as [`add_band`] does, or the top ones taken away (the band
/// left at the top keeps its own settings).
fn with_bands(mb: &Multiband, n: usize) -> Multiband {
    let n = n.clamp(2, 4);
    let mut m = mb.clone();
    while m.bands.len() > n && m.crossovers_hz.len() > 1 {
        m.crossovers_hz.pop();
        m.bands.pop();
    }
    while m.bands.len() < n {
        if !add_band(&mut m) {
            break;
        }
    }
    m
}

/// A multiband compressor: on/off and its number of bands, then a row per band (where it ends, threshold, ratio,
/// attack, release, gain, and the loud level its threshold counted from at the last render; `loud` per band, None
/// where unmeasured). Returns the new settings (None = off).
pub fn multiband(ui: &mut Ui, key: &str, mb: Option<&Multiband>, loud: &[Option<f64>]) -> Option<Option<Multiband>> {
    let mut out = None;
    let flip = on_off(ui, mb.is_some(), |ui| {
        let Some(mb) = mb else { return };
        ui.spacing_mut().item_spacing.x = 6.0;
        caption(ui, "Bands");
        let mut n = mb.bands.len();
        let counts = [(2, "2", true, "Two bands"), (3, "3", true, "Three bands"), (4, "4", true, "Four bands")];
        if segmented(ui, &format!("{key}_bands"), &mut n, &counts) {
            out = Some(Some(with_bands(mb, n)));
        }
    });
    if let Some(on) = flip {
        out = Some(on.then(|| mb.cloned().unwrap_or_default()));
    }
    let Some(mb) = mb else { return out };
    ui.add_space(6.0);
    header(ui, &[(BAND_W, "Band"), (COL_W, "To Hz"), (COL_W, "Thr dB"), (COL_W, "Ratio"), (COL_W, "Att ms"), (COL_W, "Rel ms"), (COL_W, "Gain"), (COL_W, "Loud")]);
    let names = band_names(mb.bands.len());
    let base = Id::new(key);
    for (i, b) in mb.bands.iter().enumerate() {
        let id = base.with(i);
        let name = names.get(i).copied().unwrap_or("Band");
        let set = |nb: MbBand| {
            let mut m = mb.clone();
            m.bands[i] = nb;
            Some(Some(m))
        };
        let label = |what: &str| format!("Multiband {} band {what}", name.to_lowercase());
        row(ui, |ui| {
            let (r, resp) = ui.allocate_exact_size(vec2(BAND_W, CELL_H), Sense::hover());
            ui.painter().text(Pos2::new(r.left() + 1.0, r.center().y), Align2::LEFT_CENTER, name, medium(11.0), TEXT2);
            resp.on_hover_text(band_range(mb, i));
            // where the band ends: the crossover to the next, kept half an octave from its neighbours
            match mb.crossovers_hz.get(i) {
                Some(&x) => {
                    let lo = if i == 0 { 30.0 } else { mb.crossovers_hz[i - 1] * 1.51 };
                    let hi = mb.crossovers_hz.get(i + 1).map_or(16000.0, |next| next / 1.51);
                    if let Some(v) = num(&label("crossover"), lo, hi.max(lo), cell_hz).scale(Scale::Log).show(ui, id.with("x"), x) {
                        let mut m = mb.clone();
                        m.crossovers_hz[i] = v;
                        out = Some(Some(m));
                    }
                }
                None => readout(ui, "\u{2013}", TEXT3, "The top band runs to the top of the range"),
            }
            if let Some(v) = num(&label("threshold"), -24.0, 6.0, cell_db).show(ui, id.with("thr"), b.threshold_db) {
                out = set(MbBand { threshold_db: v, ..*b });
            }
            if let Some(v) = num(&label("ratio"), 1.0, 10.0, cell_ratio).scale(Scale::Log).show(ui, id.with("ratio"), b.ratio) {
                out = set(MbBand { ratio: v, ..*b });
            }
            if let Some(v) = num(&label("attack"), 0.1, 200.0, cell_ms).scale(Scale::Log).show(ui, id.with("att"), b.attack_ms) {
                out = set(MbBand { attack_ms: v, ..*b });
            }
            if let Some(v) = num(&label("release"), 5.0, 2000.0, cell_ms).scale(Scale::Log).show(ui, id.with("rel"), b.release_ms) {
                out = set(MbBand { release_ms: v, ..*b });
            }
            if let Some(v) = num(&label("gain"), -12.0, 12.0, cell_db).show(ui, id.with("gain"), b.gain_db) {
                out = set(MbBand { gain_db: v, ..*b });
            }
            match loud.get(i).copied().flatten() {
                Some(l) => readout(ui, &format!("{l:.1}"), TEXT2, "The band's loud level at the last render (dBFS): its threshold counts from it"),
                None => readout(ui, "\u{2013}", TEXT3, "Render to measure the band's loud level, which its threshold counts from"),
            }
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// reverb

/// The built-in reverb's algorithms.
const ALGORITHMS: [Segment<'static, ReverbAlgorithm>; 4] = [
    (ReverbAlgorithm::Classic, "Classic", true, "The reverb of older projects: metallic on long tails"),
    (ReverbAlgorithm::Room, "Room", true, "A small, dense room: early reflections and a short tail"),
    (ReverbAlgorithm::Hall, "Hall", true, "A large hall: a slow build and a smooth, long tail"),
    (ReverbAlgorithm::Plate, "Plate", true, "A bright, dense plate with no early reflections"),
];

/// A reverb's algorithm. Returns the new settings.
pub fn algorithm(ui: &mut Ui, key: &str, r: ReverbParams) -> Option<ReverbParams> {
    let mut a = r.algorithm;
    segmented(ui, &format!("{key}_algorithm"), &mut a, &ALGORITHMS).then_some(ReverbParams { algorithm: a, ..r })
}

/// One of a reverb's knobs.
#[derive(Clone, Copy, Debug, Hash)]
pub enum Rv {
    Size,
    Decay,
    Predelay,
    Damping,
    Width,
    LowCut,
    HighCut,
    Level,
}

/// A reverb knob labelled `label`, `kw` wide. Returns the new settings.
pub fn reverb_knob(ui: &mut Ui, key: &str, r: ReverbParams, which: Rv, label: &str, kw: f32) -> Option<ReverbParams> {
    let d = ReverbParams::default();
    let (knob, v) = match which {
        Rv::Size => (Knob::new(label, 0.0, 1.0, d.room_size, f_pct), r.room_size),
        Rv::Decay => (Knob::new(label, 0.2, 12.0, 0.0, f_decay).scale(Scale::LogOff), r.decay_s),
        Rv::Predelay => (Knob::new(label, 0.0, 200.0, d.predelay_ms, f_ms), r.predelay_ms),
        Rv::Damping => (Knob::new(label, 0.0, 1.0, d.damping, f_pct), r.damping),
        Rv::Width => (Knob::new(label, 0.0, 1.0, d.width, f_pct), r.width),
        Rv::LowCut => (Knob::new(label, 20.0, 2000.0, d.hpf_hz, f_hz).scale(Scale::Log), r.hpf_hz),
        Rv::HighCut => (Knob::new(label, 1000.0, 20000.0, d.lpf_hz, f_hz).scale(Scale::Log), r.lpf_hz),
        Rv::Level => (Knob::new(label, -40.0, 12.0, d.return_db, f_db), r.return_db),
    };
    let x = knob.width(kw).show(ui, Id::new((key, which)), v)?;
    Some(match which {
        Rv::Size => ReverbParams { room_size: x, ..r },
        Rv::Decay => ReverbParams { decay_s: x, ..r },
        Rv::Predelay => ReverbParams { predelay_ms: x, ..r },
        Rv::Damping => ReverbParams { damping: x, ..r },
        Rv::Width => ReverbParams { width: x, ..r },
        Rv::LowCut => ReverbParams { hpf_hz: x, ..r },
        Rv::HighCut => ReverbParams { lpf_hz: x, ..r },
        Rv::Level => ReverbParams { return_db: x, ..r },
    })
}

/// A channel strip's reverb: on/off and its algorithm, then its knobs in two rows. `fresh` is what turning it on
/// starts from. Returns the new settings (None = off).
pub fn strip_reverb(ui: &mut Ui, key: &str, fx: Option<ReverbFx>, fresh: ReverbFx, kw: f32) -> Option<Option<ReverbFx>> {
    let mut out = None;
    let flip = on_off(ui, fx.is_some(), |ui| {
        if let Some(fx) = fx
            && let Some(r) = algorithm(ui, key, fx.params)
        {
            out = Some(Some(ReverbFx { params: r, ..fx }));
        }
    });
    if let Some(on) = flip {
        out = Some(on.then_some(fx.unwrap_or(fresh)));
    }
    let Some(fx) = fx else { return out };
    let mut params = None;
    let mut mix = None;
    knob_row(ui, |ui| {
        for (which, label) in [(Rv::Size, "Size"), (Rv::Decay, "Decay"), (Rv::Predelay, "Pre-delay"), (Rv::Damping, "Damping"), (Rv::Width, "Width")] {
            params = reverb_knob(ui, key, fx.params, which, label, kw).or(params);
        }
    });
    knob_row(ui, |ui| {
        for (which, label) in [(Rv::LowCut, "Low cut"), (Rv::HighCut, "High cut"), (Rv::Level, "Level")] {
            params = reverb_knob(ui, key, fx.params, which, label, kw).or(params);
        }
        mix = Knob::new("Mix", 0.0, 1.0, fresh.mix, f_pct).width(kw).show(ui, Id::new((key, "mix")), fx.mix);
    });
    if let Some(r) = params {
        out = Some(Some(ReverbFx { params: r, ..fx }));
    }
    if let Some(m) = mix {
        out = Some(Some(ReverbFx { mix: m, ..fx }));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// routing

/// Whether a track is a bus: other tracks play or send into it, or it has no instrument of its own.
pub fn is_bus(p: &Project, t: &Track) -> bool {
    matches!(t.instrument, Instrument::None) || feeds(p, t.id).next().is_some()
}

/// The tracks playing (output) or sending into track `id`, with whether it's their output.
fn feeds(p: &Project, id: u64) -> impl Iterator<Item = (&Track, bool)> {
    p.tracks.iter().filter(move |t| t.id != id).filter_map(move |t| {
        if t.mixer.output == Some(id) {
            Some((t, true))
        } else {
            t.mixer.sends.iter().any(|s| s.to == id).then_some((t, false))
        }
    })
}

/// The buses track `id` can output or send to: every bus but itself and those whose signal comes back to it.
pub fn route_targets(p: &Project, id: u64) -> Vec<&Track> {
    p.tracks.iter().filter(|t| t.id != id && is_bus(p, t) && !reaches(p, t.id, id)).collect()
}

/// Whether there are buses besides `t`.
fn other_buses(p: &Project, t: &Track) -> bool {
    p.tracks.iter().any(|b| b.id != t.id && is_bus(p, b))
}

/// Whether a track is an aux return: tracks send to it, and none plays into it as their output.
pub fn aux_return(p: &Project, t: &Track) -> bool {
    let mut f = feeds(p, t.id).peekable();
    f.peek().is_some() && f.all(|x| !x.1)
}

/// A track's name, or a placeholder for a reference to a track that's gone.
pub fn name_of(p: &Project, id: u64) -> &str {
    p.tracks.iter().find(|t| t.id == id).map_or("(missing track)", |t| t.name.as_str())
}

/// The menu of a track's output field: the master, then the buses it can go to. Returns the change.
pub fn output_menu(field: &Response, p: &Project, t: &Track) -> Option<MixerPatch> {
    let mut out = None;
    Popup::menu(field).show(|ui| {
        let to = |o: Option<TrackRef>| Some(MixerPatch { output: Some(o), ..Default::default() });
        if menu_item(ui, MENU_W, "Master", "", Some(t.mixer.output.is_none()), true) && t.mixer.output.is_some() {
            out = to(None);
        }
        let targets = route_targets(p, t.id);
        if targets.is_empty() {
            let text = if other_buses(p, t) { "The other buses feed this one" } else { "No buses yet: add one in the mixer" };
            menu_item(ui, MENU_W, text, "", None, false);
        }
        for b in targets {
            let on = t.mixer.output == Some(b.id);
            if menu_item(ui, MENU_W, &b.name, "", Some(on), true) && !on {
                out = to(Some(TrackRef::Id(b.id)));
            }
        }
    });
    out
}

/// A track's routing: where its output goes (the master or a bus), its sends to buses with their levels, and for a
/// bus, what plays into it. Only buses that wouldn't make the signal go round in a loop are offered. Returns the
/// change.
pub fn routing(ui: &mut Ui, key: &str, p: &Project, t: &Track, width: f32) -> Option<MixerPatch> {
    let targets = route_targets(p, t.id);
    let base = Id::new(key);

    caption(ui, "Output");
    ui.add_space(2.0);
    let current = t.mixer.output.map_or("Master", |o| name_of(p, o));
    let resp = chip(ui, width, current, true, &format!("Output, {current}")).on_hover_text("Where this track's sound goes: the master or a bus");
    let mut out = output_menu(&resp, p, t);

    ui.add_space(10.0);
    caption(ui, "Sends");
    ui.add_space(2.0);
    let specs = |list: Vec<(u64, f64)>| MixerPatch {
        sends: Some(list.into_iter().map(|(to, level)| SendSpec { to: TrackRef::Id(to), level }).collect()),
        ..Default::default()
    };
    let sends: Vec<(u64, f64)> = t.mixer.sends.iter().map(|s| (s.to, s.level)).collect();
    if sends.is_empty() {
        caption(ui, "None");
    }
    let name_w = (width - COL_W - DEL_W - 2.0 * GAP).max(40.0);
    for (i, &(to, level)) in sends.iter().enumerate() {
        let name = name_of(p, to);
        row(ui, |ui| {
            let (r, resp) = ui.allocate_exact_size(vec2(name_w, CELL_H), Sense::hover());
            let g = one_line(ui, name, medium(11.0), TEXT2, name_w - 2.0);
            let at = Align2::LEFT_CENTER.anchor_size(Pos2::new(r.left() + 1.0, r.center().y), g.size()).min;
            ui.painter().galley(at, g, TEXT2);
            resp.on_hover_text(format!("Sends to {name}, after the fader"));
            let label = format!("Send to {name} level");
            if let Some(v) = num(&label, 0.0, 1.0, cell_pct).parse(parse_pct).show(ui, base.with(("send", to)), level) {
                let mut list = sends.clone();
                list[i].1 = v;
                out = Some(specs(list));
            }
            if remove(ui, "Remove the send (undo brings it back)") {
                let mut list = sends.clone();
                list.remove(i);
                out = Some(specs(list));
            }
        });
    }
    let open: Vec<&&Track> = targets.iter().filter(|b| !sends.iter().any(|s| s.0 == b.id)).collect();
    ui.add_space(4.0);
    let hint = match (open.is_empty(), other_buses(p, t)) {
        (false, _) => "Send some of this track's sound to a bus too",
        (true, true) => "It sends to every bus it can already",
        (true, false) => "Add a bus in the mixer to send to it",
    };
    let resp = chip(ui, width, "Add a send", !open.is_empty(), "Add a send").on_hover_text(hint);
    Popup::menu(&resp).show(|ui| {
        for b in &open {
            if menu_item(ui, MENU_W, &b.name, "", None, true) {
                let mut list = sends.clone();
                list.push((b.id, 0.25));
                out = Some(specs(list));
            }
        }
    });

    // what plays into a bus
    let (outputs, sending): (Vec<_>, Vec<_>) = feeds(p, t.id).partition(|f| f.1);
    let outputs: Vec<&str> = outputs.iter().map(|f| f.0.name.as_str()).collect();
    let sending: Vec<&str> = sending.iter().map(|f| f.0.name.as_str()).collect();
    let mut lines = vec![];
    if !outputs.is_empty() {
        lines.push(format!("{} {} into this bus.", and_list(&outputs), if outputs.len() == 1 { "plays" } else { "play" }));
    }
    if !sending.is_empty() {
        lines.push(format!("{} {} to it.", and_list(&sending), if sending.len() == 1 { "sends" } else { "send" }));
    }
    if !lines.is_empty() {
        ui.add_space(10.0);
        ui.add(egui::Label::new(RichText::new(lines.join(" ")).font(body(11.0)).color(TEXT3)).wrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::AuxSend;

    fn mb(x: &[f64]) -> Multiband {
        let band = MbBand { threshold_db: -3.0, ratio: 2.0, attack_ms: 10.0, release_ms: 100.0, gain_db: 0.0 };
        Multiband { crossovers_hz: x.to_vec(), bands: vec![band; x.len() + 1] }
    }

    #[test]
    fn bands_are_added_above_below_or_in_the_widest_gap() {
        // above the top crossover while there's room
        assert_eq!(with_bands(&mb(&[200.0]), 3).crossovers_hz, vec![200.0, 600.0]);
        // below the lowest when the top is full
        assert_eq!(with_bands(&mb(&[1000.0, 9000.0]), 4).crossovers_hz, vec![1000.0 / 3.0, 1000.0, 9000.0]);
        // in the widest gap when both ends are full
        let m = with_bands(&mb(&[60.0, 6000.0]), 4);
        assert_eq!(m.crossovers_hz.len(), 3);
        assert!((m.crossovers_hz[1] - 600.0).abs() < 1e-6);
        assert_eq!(m.bands.len(), 4);
        // taking bands away drops the top ones
        let m = with_bands(&mb(&[150.0, 2500.0, 8000.0]), 2);
        assert_eq!(m.crossovers_hz, vec![150.0]);
        assert_eq!(m.bands.len(), 2);
    }

    #[test]
    fn asking_for_more_bands_adds_each_in_turn() {
        assert_eq!(with_bands(&mb(&[150.0]), 4).crossovers_hz, vec![150.0, 450.0, 1350.0]);
        let full = mb(&[35.0, 60.0, 12000.0]);
        assert_eq!(with_bands(&full, 4), full);
        assert_eq!(with_bands(&full, 9), full);
    }

    #[test]
    fn a_new_band_avoids_the_bands_already_there() {
        assert_eq!(free_hz(&[]), 1000.0);
        assert_eq!(free_hz(&[1000.0]), 250.0);
        assert_eq!(free_hz(&[900.0, 260.0]), 4000.0);
    }

    #[test]
    fn routing_offers_only_buses_that_dont_lead_back() {
        let mut p = Project::new("t", 120.0, 4);
        for n in ["Drums", "Group", "FX", "Pad"] {
            flux_core::commands::apply(&mut p, &flux_core::Command::AddTrack { name: n.into(), instrument: Instrument::None, color: None }).unwrap();
        }
        let id = |n: &str| p.tracks.iter().find(|t| t.name == n).unwrap().id;
        let (drums, group, fx, pad) = (id("Drums"), id("Group"), id("FX"), id("Pad"));
        p.tracks[0].instrument = Instrument::Sfz { path: "drums.sfz".into(), init_cc: Default::default() };
        p.tracks[3].instrument = Instrument::Sfz { path: "pad.sfz".into(), init_cc: Default::default() };
        p.tracks[0].mixer.output = Some(group);
        p.tracks[3].mixer.sends = vec![AuxSend { to: fx, level: 0.3 }];
        let names = |v: Vec<&Track>| v.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        // instrument tracks nothing plays into aren't buses
        assert_eq!(names(route_targets(&p, group)), vec!["FX"]);
        assert_eq!(names(route_targets(&p, drums)), vec!["Group", "FX"]);
        assert_eq!(names(route_targets(&p, pad)), vec!["Group", "FX"]);
        // a bus playing into another isn't offered the other way round
        p.tracks[1].mixer.output = Some(fx);
        assert_eq!(names(route_targets(&p, fx)), Vec::<String>::new());
        assert!(is_bus(&p, &p.tracks[1]) && !is_bus(&p, &p.tracks[3]));
    }

    #[test]
    fn names_read_as_a_sentence() {
        assert_eq!(and_list(&["Harp"]), "Harp");
        assert_eq!(and_list(&["Harp", "Flute"]), "Harp and Flute");
        assert_eq!(and_list(&["Harp", "Flute", "Pad"]), "Harp, Flute and Pad");
    }
}
