//! Bottom panel: mixer, track inspector, master and analysis.

use super::arrange::{engine_state, kind_label, section_color};
use super::theme::*;
use super::widgets::*;
use super::{Action, Feed, Gui, Tab, effects, master_cmd, mixer_cmd, mmss};
use crate::state::{LastRender, State};
use flux_analysis::master::{MasterMove, MasterReport, Part, Saturation, TonalBalance, TonalStyle, phase_limits};
use flux_analysis::matcheq::EqShape;
use flux_core::commands::{Bus, Command, MasterPatch, MixerPatch};
use flux_core::{Chain, ChorusFx, Comp, EqPhase, Insert, InsertPosition, Instrument, Project, ReverbFx, ReverbParams, Track, TrackRef};
use flux_engine::render::TrackStat;
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, vec2};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub(super) struct AnalysisCache {
    seq: u64,
    bars: Vec<f64>,
    bands: Vec<(String, f64)>,
}

/// A master check: its report, or why it failed.
type Checked = Result<Arc<MasterReport>, String>;

/// The master check the analysis shows: the agent's last check of the render, or one run here against the style
/// picked (on a thread of its own, so the window doesn't wait for it).
#[derive(Default)]
pub(super) struct MasterView {
    /// The style picked here; None shows the agent's last check, else a check against the balanced masters.
    style: Option<TonalStyle>,
    /// The render (by seq) and the style picked that `shown`'s check is for, and the check.
    shown: Option<(u64, Option<TonalStyle>, Checked)>,
    /// A check running: the render and the style picked it's for.
    running: Option<(u64, Option<TonalStyle>, JoinHandle<anyhow::Result<MasterReport>>)>,
    /// Run checks on the GUI thread instead (tests: a snapshot can't wait for a thread).
    pub(super) inline: bool,
}

/// The styles a master is checked against, as their menu lists them.
const STYLES: [(TonalStyle, &str, &str); 4] = [
    (TonalStyle::Balanced, "Balanced", "band-based game and pop"),
    (TonalStyle::Cinematic, "Cinematic", "orchestral and ambient"),
    (TonalStyle::Electronic, "Electronic", "synths and beats"),
    (TonalStyle::Acoustic, "Acoustic", "small acoustic groups"),
];

fn style_name(s: TonalStyle) -> &'static str {
    STYLES.iter().find(|x| x.0 == s).map_or("Balanced", |x| x.1)
}

/// A frequency in words: "250 Hz", "2.5 kHz".
fn hz_words(v: f64) -> String {
    if v >= 1000.0 { format!("{} kHz", (v / 100.0).round() / 10.0) } else { format!("{v:.0} Hz") }
}

/// A master move in words, for a finding's tooltip.
fn move_text(m: &MasterMove) -> String {
    let on = |p: Part| match p {
        Part::Stereo => "",
        Part::Mid => " on the mid",
        Part::Side => " on the side",
    };
    let shape = |s: EqShape| match s {
        EqShape::Peak => "a peak",
        EqShape::LowShelf => "a low shelf",
        EqShape::HighShelf => "a high shelf",
    };
    match *m {
        MasterMove::Eq { shape: s, hz, db, q, part } => format!("EQ {} at {}, {db:+.1} dB, Q {q:.2}{}", shape(s), hz_words(hz), on(part)),
        MasterMove::HighPass { hz, slope } => format!("EQ high-pass at {}, {slope} dB per octave", hz_words(hz)),
        MasterMove::MonoBelow { hz } => format!("mono below {}", hz_words(hz)),
        MasterMove::Width { width } => format!("width times {width:.2}"),
        MasterMove::DeEss { hz, range_db } => format!("de-esser above {}, range {range_db:.1} dB", hz_words(hz)),
        MasterMove::DynEq { shape: s, hz, q, range_db } => format!("dynamic EQ: {} at {}, Q {q:.2}, range {range_db:.1} dB", shape(s), hz_words(hz)),
        MasterMove::Exciter { hz_lo, hz_hi, mix, mode } => {
            let curve = if mode == Saturation::Tube { "tube" } else { "tape" };
            format!("exciter from {} to {}, {:.0}% {curve}", hz_words(hz_lo), hz_words(hz_hi), mix * 100.0)
        }
        MasterMove::Imager { hz_lo, hz_hi, width } => format!("imager: {} to {} at {:.0}% width", hz_words(hz_lo), hz_words(hz_hi), width * 100.0),
        MasterMove::Loudness { target_lufs } => format!("loudness target {target_lufs:.1} LUFS"),
        MasterMove::Limiter { release_ms, clip_db } => {
            let mut parts = vec![];
            if let Some(r) = release_ms {
                parts.push(format!("release {r:.0} ms"));
            }
            if let Some(c) = clip_db {
                parts.push(format!("clip {c:.1} dB"));
            }
            format!("limiter {}", parts.join(", "))
        }
    }
}

/// How far the tonal plot reaches above and below the target (dB).
const TONAL_SPAN_DB: f64 = 12.0;

/// The tonal balance, `h` tall: each 1/3-octave band's deviation from the target (the line) and its tolerance (the
/// shaded band around 0); the bands outside it are marked.
fn tonal_plot(ui: &mut egui::Ui, t: &TonalBalance, h: f32) {
    let (full, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let rect = Rect::from_min_max(Pos2::new(full.left() + 24.0, full.top()), Pos2::new(full.right(), full.bottom() - 16.0));
    let p = ui.painter();
    p.rect_filled(rect, 2.0, FIELD);
    let y_of = |d: f64| rect.center().y - (d.clamp(-TONAL_SPAN_DB, TONAL_SPAN_DB) / TONAL_SPAN_DB) as f32 * rect.height() * 0.5;
    let n = t.bands.len().max(1);
    let bw = rect.width() / n as f32;
    let x_of = |i: usize| rect.left() + (i as f32 + 0.5) * bw;
    let corridor = FIELD.lerp_to_gamma(ACCENT_TINT, 0.75);
    for (i, b) in t.bands.iter().enumerate() {
        let x0 = rect.left() + i as f32 * bw;
        p.rect_filled(Rect::from_min_max(Pos2::new(x0, y_of(b.tolerance_db)), Pos2::new(x0 + bw, y_of(-b.tolerance_db))), 0.0, corridor);
    }
    for g in [-6.0, 0.0, 6.0] {
        let y = px(y_of(g));
        p.hline(rect.x_range(), y, Stroke::new(1.0, if g == 0.0 { LINE_STRONG } else { LINE_SOFT }));
        let label = if g == 0.0 { "0".to_string() } else { format!("{g:+.0}") };
        p.text(Pos2::new(rect.left() - 5.0, y), Align2::RIGHT_CENTER, label, mono(9.0), TEXT3);
    }
    let points: Vec<Pos2> = t.bands.iter().enumerate().filter_map(|(i, b)| b.deviation_db.map(|d| Pos2::new(x_of(i), y_of(d)))).collect();
    p.add(egui::Shape::line(points, Stroke::new(1.5, ACCENT)));
    for (i, b) in t.bands.iter().enumerate() {
        if let (true, Some(d)) = (b.outside, b.deviation_db) {
            p.circle_filled(Pos2::new(x_of(i), y_of(d)), 2.5, WARN);
        }
    }
    for (f, label) in [(63.0, "63"), (125.0, "125"), (250.0, "250"), (500.0, "500"), (1000.0, "1k"), (2000.0, "2k"), (4000.0, "4k"), (8000.0, "8k"), (16000.0, "16k")] {
        if let Some(i) = t.bands.iter().position(|b: &flux_analysis::master::TonalBand| (b.centre_hz / f).log2().abs() < 0.1) {
            p.text(Pos2::new(x_of(i), full.bottom()), Align2::CENTER_BOTTOM, label, body(10.0), TEXT3);
        }
    }
    if let Some(at) = resp.hover_pos() {
        let i = (((at.x - rect.left()).max(0.0) / bw) as usize).min(n - 1);
        if let Some(b) = t.bands.get(i) {
            let text = match b.deviation_db {
                Some(d) => format!("{}: {d:+.1} dB from the target (tolerance \u{b1}{:.1} dB)", hz_words(b.centre_hz), b.tolerance_db),
                None => format!("{}: too quiet to measure", hz_words(b.centre_hz)),
            };
            resp.on_hover_text(text);
        }
    }
}

/// A band's correlation on a bar from -1 to 1: filled from 0, blue while it's above the band's phase warning limit
/// (the tick), else coloured as a warning or a problem.
fn correlation_bar(p: &egui::Painter, r: Rect, c: f64, (warn, problem): (f64, f64)) {
    p.rect_filled(r, 2.0, FIELD);
    let x_of = |v: f64| r.left() + ((v.clamp(-1.0, 1.0) + 1.0) * 0.5) as f32 * r.width();
    let (a, b) = (x_of(0.0), x_of(c));
    let fill = if c >= warn {
        ACCENT
    } else if c >= problem {
        WARN
    } else {
        ERR
    };
    p.rect_filled(Rect::from_min_max(Pos2::new(a.min(b), r.top() + 3.0), Pos2::new(a.max(b).max(a.min(b) + 1.0), r.bottom() - 3.0)), 1.0, fill);
    p.vline(px(a), r.y_range(), Stroke::new(1.0, LINE_STRONG));
    p.vline(px(x_of(warn)), r.y_range().shrink(2.0), Stroke::new(1.0, TEXT3));
}

/// A sub-panel of the analysis, `w` wide.
fn analysis_panel(ui: &mut egui::Ui, w: f32, pad: Margin, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().inner_margin(pad).show(ui, |ui| {
        ui.set_width(w - 32.0);
        ui.vertical(add);
    });
}

/// A sub-panel's title, with `right` at its right end (all the titles of a row line up, with a control or not).
fn panel_head(ui: &mut egui::Ui, name: &str, right: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(CELL_H);
        title(ui, name);
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
}

/// A vertical hairline between sub-panels, `h` tall.
fn panel_sep(ui: &mut egui::Ui, h: f32) {
    let (sep, _) = ui.allocate_exact_size(vec2(1.0, h), Sense::hover());
    ui.painter().vline(px(sep.left()), sep.y_range(), Stroke::new(1.0, LINE_SOFT));
}

/// What a sub-panel shows while its check runs, or when it failed.
fn check_pending(ui: &mut egui::Ui, rep: &Option<Checked>) {
    match rep {
        None => caption(ui, "Checking the master\u{2026}"),
        Some(Err(e)) => caption(ui, &format!("The check failed: {e}")),
        Some(Ok(_)) => caption(ui, ""),
    };
}

const STRIP_W: f32 = 84.0;
const MASTER_W: f32 = 252.0;

fn insert_names(t: &Track) -> Vec<String> {
    let m = &t.mixer;
    let mut v: Vec<String> = m.inserts.iter().filter(|i| !i.bypass).map(|i| i.plugin_name().to_string()).collect();
    let slope = |s: u32| if s == 12 { String::new() } else { format!(", {s} dB") };
    if m.hpf_hz > 0.0 {
        v.push(format!("HPF {}{}", f_hz(m.hpf_hz), slope(m.hpf_slope)));
    }
    if m.lpf_hz > 0.0 {
        v.push(format!("LPF {}{}", f_hz(m.lpf_hz), slope(m.lpf_slope)));
    }
    if m.mono_below_hz > 0.0 {
        v.push(format!("Mono < {}", f_hz(m.mono_below_hz)));
    }
    if !m.eq.is_empty() {
        v.push(format!("EQ \u{d7}{}", m.eq.len()));
    }
    if m.drive_db > 0.0 {
        v.push(format!("Drive {:.0}", m.drive_db));
    }
    if let Some(a) = &m.amp {
        v.push(format!("Amp {:.0}", a.gain_db));
    }
    if m.chorus.is_some() {
        v.push("Chorus".into());
    }
    if m.bitcrush_bits > 0.0 {
        v.push(format!("Crush {:.0}", m.bitcrush_bits));
    }
    if m.clip_db.is_some() {
        v.push("Clip".into());
    }
    if m.gate.is_some() {
        v.push("Gate".into());
    }
    if m.comp.is_some() {
        v.push("Comp".into());
    }
    if !m.dyn_eq.is_empty() {
        v.push(format!("Dyn EQ \u{d7}{}", m.dyn_eq.len()));
    }
    if m.deess.is_some() {
        v.push("De-ess".into());
    }
    if m.delay.is_some() {
        v.push("Delay".into());
    }
    if m.reverb.is_some() {
        v.push("Reverb".into());
    }
    if m.duck.is_some() {
        v.push("Duck".into());
    }
    v
}

/// A label and a value on one line, with an optional OK / warning dot.
fn stat_row(ui: &mut egui::Ui, label: &str, value: String, ok: Option<bool>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(vec2(6.0, 16.0), Sense::hover());
        if let Some(ok) = ok {
            ui.painter().circle_filled(r.center(), 3.0, if ok { OK } else { WARN });
        }
        ui.label(RichText::new(label).font(body(11.5)).color(TEXT2));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).font(mono(11.0)).color(TEXT));
        });
    });
}

/// A section of an inspector tab: a padded column with a title, closed by a hairline on its right.
fn section(ui: &mut egui::Ui, width: f32, head: &str, sub: &str, add: impl FnOnce(&mut egui::Ui)) {
    let bottom = ui.max_rect().bottom();
    let r = egui::Frame::new()
        .inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 14 })
        .show(ui, |ui| {
            ui.set_width(width);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.label(RichText::new(head).font(semibold(12.0)).color(TEXT));
                if !sub.is_empty() {
                    ui.label(RichText::new(sub).font(body(11.0)).color(TEXT3));
                }
                ui.add_space(8.0);
                add(ui);
            });
        })
        .response
        .rect;
    ui.painter().vline(px(r.right()) - 1.0, r.top()..=bottom.max(r.bottom()), Stroke::new(1.0, LINE_SOFT));
}

/// Width of an inserts section.
const INSERTS_W: f32 = 300.0;

/// A section's inner margins, left and right.
const SECTION_PAD: f32 = 32.0;

/// Width of the track tab's channel strip: its six knobs on top, and below them four more and the switches.
const CHANNEL_W: f32 = 380.0;

/// A channel-strip knob's width (the default for its 30 px dial).
const STRIP_KNOB_W: f32 = 52.0;

/// A cut's slope: a chip as wide as the cut's knob above it (a combo box is wider), opening a menu; greyed while the
/// cut is off. Returns a new pick.
fn slope_menu(ui: &mut egui::Ui, slope: u32, on: bool, which: &str) -> Option<u32> {
    let resp = chip(ui, STRIP_KNOB_W, &format!("{slope} dB"), on, &format!("{which} cut slope, {slope} dB per octave"));
    let resp = resp.on_hover_text(if on { format!("The {which} cut's slope: {slope} dB per octave") } else { format!("Turn the {which} cut on first") });
    menu_pick(&resp, slope, &effects::SLOPES, 180.0)
}

/// Widths for the sections of an inspector tab, each given as (narrowest, full): every section gets its full width
/// when the panel has room, and when it is narrower the flexible ones give up width, in proportion to what they can
/// give, down to their narrowest. When even that doesn't fit, the tab scrolls and every section keeps its full width.
fn fit_widths(avail: f32, sections: &[(f32, f32)]) -> Vec<f32> {
    let full: f32 = sections.iter().map(|s| s.1 + SECTION_PAD).sum();
    let slack: f32 = sections.iter().map(|s| s.1 - s.0).sum();
    let short = full - avail;
    if short <= 0.0 || short > slack {
        return sections.iter().map(|s| s.1).collect();
    }
    sections.iter().map(|&(min, max)| (max - short * (max - min) / slack).floor()).collect()
}

/// Shows a panel's content in a scroll area whose bar stays faintly visible while the content runs past the edge (the
/// theme's floating bars only appear on hover), so the sections or strips out of sight can be found.
fn scroll_hinted<R>(ui: &mut egui::Ui, area: egui::ScrollArea, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let normal = ui.spacing().scroll;
    ui.scope(|ui| {
        ui.spacing_mut().scroll.dormant_handle_opacity = 0.5;
        area.show(ui, |ui| {
            // scroll areas inside keep the usual bars
            ui.spacing_mut().scroll = normal;
            add(ui)
        })
        .inner
    })
    .inner
}

/// A section that is as wide when the panel is narrow as when it is wide.
fn fixed(w: f32) -> (f32, f32) {
    (w, w)
}

/// An effect's section width: `w` while it's on, narrow while it's off.
fn effect_w(on: bool, w: f32) -> (f32, f32) {
    fixed(if on { w.max(effects::OFF_W) } else { effects::OFF_W })
}

/// The rows of an insert chain, in processing order (a track's pre inserts first).
fn chain_rows(chain: &[Insert]) -> Vec<&Insert> {
    let mut rows: Vec<(usize, &Insert)> = chain.iter().enumerate().collect();
    rows.sort_by_key(|(i, x)| (x.position == InsertPosition::Post, *i));
    rows.into_iter().map(|(_, x)| x).collect()
}

/// An insert chain: each effect with its bypass switch, window, order and remove buttons, then a menu adding one.
fn inserts_ui(ui: &mut egui::Ui, p: &Project, chain: Chain, act: &mut Vec<Action>) {
    let list: &[Insert] = p.chain(chain).map(Vec::as_slice).unwrap_or_default();
    let on_track = matches!(chain, Chain::Track(_));
    let rows = chain_rows(list);
    if rows.is_empty() {
        caption(ui, "No effects yet");
    }
    // the chain index of a neighbour in the same group (moving past a group boundary changes nothing)
    let index_of = |id: u64| list.iter().position(|x| x.id == id);
    for (k, ins) in rows.iter().enumerate() {
        let id = ins.id;
        let same = |j: usize| rows.get(j).filter(|o| o.position == ins.position).and_then(|o| index_of(o.id));
        let up = k.checked_sub(1).and_then(same);
        let down = same(k + 1);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let mut on = !ins.bypass;
            if switch(ui, &mut on, "") {
                act.push(Action::Apply(vec![update_insert(id, |c| {
                    if let Command::UpdateInsert { bypass, .. } = c {
                        *bypass = Some(!on);
                    }
                })]));
            }
            if on_track {
                let (label, next) = match ins.position {
                    InsertPosition::Pre => ("Pre", InsertPosition::Post),
                    InsertPosition::Post => ("Post", InsertPosition::Pre),
                };
                let g = ui.painter().layout_no_wrap(label.to_string(), medium(10.0), Color32::PLACEHOLDER);
                let (r, resp) = ui.allocate_exact_size(vec2(32.0, 18.0), Sense::click());
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("{label}-strip insert")));
                ui.painter().rect(r, CornerRadius::same(R_CTRL), if resp.hovered() { HOVER } else { FIELD }, Stroke::new(1.0, LINE), StrokeKind::Inside);
                ui.painter().galley(r.center() - g.size() * 0.5, g, TEXT2);
                let tip = if ins.position == InsertPosition::Pre {
                    "Before the strip's filters, EQ and drive. Click to move it after them"
                } else {
                    "After the strip's effects, before pan and sends. Click to move it before them"
                };
                if resp.on_hover_text(tip).clicked() {
                    act.push(Action::Apply(vec![update_insert(id, |c| {
                        if let Command::UpdateInsert { position, .. } = c {
                            *position = Some(next);
                        }
                    })]));
                }
            }
            let name_w = ui.available_width() - 4.0 * 24.0;
            let (nr, _) = ui.allocate_exact_size(vec2(name_w.max(40.0), 18.0), Sense::hover());
            let mut nu = ui.new_child(UiBuilder::new().max_rect(nr).layout(Layout::left_to_right(Align::Center)));
            nu.spacing_mut().item_spacing.x = 5.0;
            let tc = if ins.bypass { TEXT3 } else { TEXT };
            nu.add(egui::Label::new(RichText::new(ins.plugin_name()).font(medium(11.5)).color(tc)).truncate()).on_hover_text(&ins.path);
            if !ins.preset_name.is_empty() {
                nu.add(egui::Label::new(RichText::new(&ins.preset_name).font(body(11.0)).color(TEXT3)).truncate());
            }
            if icon_button_ex(ui, Icon::More, false, true, 20.0, "Open the plugin's window (changes are saved to the project)").clicked() {
                act.push(Action::OpenInsert(id));
            }
            if icon_button_ex(ui, Icon::Up, false, up.is_some(), 20.0, "Earlier in the chain").clicked()
                && let Some(i) = up
            {
                act.push(Action::Apply(vec![Command::MoveInsert { insert: id, index: i }]));
            }
            if icon_button_ex(ui, Icon::Down, false, down.is_some(), 20.0, "Later in the chain").clicked()
                && let Some(i) = down
            {
                act.push(Action::Apply(vec![Command::MoveInsert { insert: id, index: i }]));
            }
            if icon_button_ex(ui, Icon::Close, false, true, 20.0, "Remove (undo brings it back)").clicked() {
                act.push(Action::Apply(vec![Command::RemoveInsert { insert: id }]));
            }
        });
    }
    ui.add_space(6.0);
    let salt = match chain {
        Chain::Track(t) => format!("add_fx_{t}"),
        Chain::Master => "add_fx_master".into(),
        Chain::Reverb => "add_fx_reverb".into(),
    };
    egui::ComboBox::from_id_salt(salt).width(ui.available_width()).selected_text(RichText::new("Add effect\u{2026}").font(body(12.0)).color(TEXT2)).show_ui(ui, |ui| {
        // installed effects are looked for once, the first time a menu opens
        let ctx = ui.ctx().clone();
        let Some(list) = crate::plugins::effects(move || ctx.request_repaint()) else {
            ui.label(RichText::new("Looking for installed plugins\u{2026}").font(body(11.5)).color(TEXT3));
            return;
        };
        if list.is_empty() {
            ui.label(RichText::new("No VST3 effects installed").font(body(11.5)).color(TEXT3));
        }
        let mut vendor = "";
        for e in list.iter() {
            if e.vendor != vendor {
                vendor = &e.vendor;
                ui.label(RichText::new(if vendor.is_empty() { "Other" } else { vendor }).font(semibold(11.0)).color(TEXT3));
            }
            if ui.selectable_label(false, RichText::new(&e.name).font(body(12.0))).on_hover_text(&e.path).clicked() {
                let (track, bus) = match chain {
                    Chain::Track(t) => (Some(TrackRef::Id(t)), None),
                    Chain::Master => (None, Some(Bus::Master)),
                    Chain::Reverb => (None, Some(Bus::Reverb)),
                };
                act.push(Action::Apply(vec![Command::AddInsert {
                    track,
                    bus,
                    path: e.path.clone(),
                    class: None,
                    params: Default::default(),
                    preset: None,
                    preset_name: String::new(),
                    bypass: false,
                    position: None,
                    index: None,
                }]));
            }
        }
    });
}

/// An update_insert changing nothing, then `f`.
fn update_insert(id: u64, f: impl FnOnce(&mut Command)) -> Command {
    let mut c = Command::UpdateInsert { insert: id, params: None, clear_params: false, preset: None, preset_name: None, bypass: None, position: None };
    f(&mut c);
    c
}

/// A boxed numeric readout (mixer gain and peak).
fn value_box(ui: &egui::Ui, r: Rect, text: &str, color: Color32, fill: Color32) {
    let p = ui.painter();
    p.rect(r, CornerRadius::same(2), fill, Stroke::new(1.0, LINE), StrokeKind::Inside);
    p.text(r.center(), Align2::CENTER_CENTER, text, mono(10.0), color);
}

/// The mixer's form for a new bus: its name and the tracks to route into it.
#[derive(Default)]
pub(super) struct BusForm {
    name: String,
    tracks: Vec<u64>,
}

/// The first of "Bus 1", "Bus 2"... that no track is called.
fn free_bus_name(p: &Project) -> String {
    (1..).map(|n| format!("Bus {n}")).find(|n| p.tracks.iter().all(|t| t.name != *n)).unwrap_or_default()
}

/// A bus strip's name, tagged BUS and centred; it selects the bus like a track's name does.
fn bus_name(ui: &mut egui::Ui, t: &Track, color: Color32) -> egui::Response {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 16.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &t.name));
    let tag = ui.painter().layout_no_wrap("BUS".into(), semibold(8.0), TEXT2);
    let tag_w = (tag.size().x + 6.0).round();
    let g = one_line(ui, &t.name, medium(11.5), color, (w - tag_w - 4.0).max(8.0));
    let x0 = (r.center().x - (tag_w + 4.0 + g.size().x) * 0.5).round();
    let tr = Rect::from_min_size(Pos2::new(x0, (r.center().y - 6.0).round()), vec2(tag_w, 12.0));
    let p = ui.painter();
    p.rect_stroke(tr, 2.0, Stroke::new(1.0, TEXT3), StrokeKind::Inside);
    p.galley(tr.center() - tag.size() * 0.5, tag, TEXT2);
    p.galley(Pos2::new(tr.right() + 4.0, (r.center().y - g.size().y * 0.5).round()), g, color);
    resp.on_hover_text(format!("{}: a bus. Other tracks play or send into it", t.name))
}

/// A strip's routing row: its output, as a field opening the output menu, and a small bar per send showing its level.
/// Returns a change of output.
fn strip_io(ui: &mut egui::Ui, p: &Project, t: &Track, row: Rect) -> Option<MixerPatch> {
    let sends = &t.mixer.sends;
    let shown = sends.len().min(4);
    let bars_w = if shown == 0 { 0.0 } else { shown as f32 * 5.0 + 2.0 };
    let fr = Rect::from_min_max(row.min, Pos2::new(row.right() - bars_w, row.bottom()));
    let out = t.mixer.output.map(|o| effects::name_of(p, o));
    let text = out.unwrap_or("Master");
    let resp = ui.interact(fr, Id::new(("strip_out", t.id)), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, format!("{} output, {text}", t.name)));
    let pnt = ui.painter();
    pnt.rect(fr, CornerRadius::same(R_CTRL), if resp.hovered() { HOVER } else { FIELD }, Stroke::new(1.0, LINE), StrokeKind::Inside);
    let c = if out.is_some() { TEXT2 } else { TEXT3 };
    let g = one_line(ui, text, medium(10.0), c, fr.width() - 10.0);
    pnt.galley(Pos2::new(fr.left() + 5.0, (fr.center().y - g.size().y * 0.5).round()), g, c);
    for (k, s) in sends.iter().take(shown).enumerate() {
        let x = fr.right() + 4.0 + k as f32 * 5.0;
        let br = Rect::from_min_max(Pos2::new(x, row.top() + 2.0), Pos2::new(x + 3.0, row.bottom() - 2.0));
        pnt.rect_filled(br, 1.0, WELL);
        let h = (br.height() * s.level.clamp(0.0, 1.0) as f32).round();
        pnt.rect_filled(Rect::from_min_max(Pos2::new(br.left(), br.bottom() - h), br.max), 1.0, ACCENT);
    }
    if shown > 0 {
        let list: Vec<String> = sends.iter().map(|s| format!("{} {}", effects::name_of(p, s.to), f_pct(s.level))).collect();
        let bars = Rect::from_min_max(Pos2::new(fr.right() + 2.0, row.top()), row.max);
        ui.interact(bars, Id::new(("strip_sends", t.id)), Sense::hover()).on_hover_text(format!("Sends: {}", list.join(", ")));
    }
    let resp = resp.on_hover_text(format!("Output: {text}. Click to change it"));
    effects::output_menu(&resp, p, t)
}

/// A destructive button that asks twice: the first click arms it.
fn danger_button(ui: &mut egui::Ui, text: &str, armed: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), medium(12.0), Color32::PLACEHOLDER);
    let (r, resp) = ui.allocate_exact_size(vec2((g.size().x + 24.0).round(), 24.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    let hot = resp.hovered();
    let (fill, stroke, tc) = if armed {
        (if hot { ERR.lerp_to_gamma(Color32::WHITE, 0.1) } else { ERR }, Stroke::NONE, Color32::WHITE)
    } else {
        (if hot { HOVER } else { RAISED }, Stroke::new(1.0, LINE_STRONG), ERR.lerp_to_gamma(Color32::WHITE, 0.2))
    };
    let p = ui.painter();
    p.rect(r, CornerRadius::same(R_CTRL), fill, stroke, StrokeKind::Inside);
    p.galley(Pos2::new(r.left() + 12.0, (r.center().y - g.size().y * 0.5).round()), g, tc);
    resp
}

impl Gui {
    pub(super) fn bottom(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let mut tab = self.tab;
        let sel = self.selected.and_then(|id| st.session.project.tracks.iter().find(|t| t.id == id));
        header_bar(
            ui,
            |ui| {
                tab_strip(ui, &mut tab, &[(Tab::Mixer, "Mixer", None), (Tab::Track, "Track", None), (Tab::Master, "Master", None), (Tab::Analysis, "Analysis", None)]);
            },
            |ui| {
                if self.tab == Tab::Mixer {
                    self.bus_button(ui, &st.session.project, act);
                    if sel.is_some() {
                        vsep(ui, 16.0);
                    }
                }
                if let Some(t) = sel {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(&t.name).font(medium(12.0)).color(TEXT));
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter().rect_filled(r, 2.0, col(t.color));
                    ui.label(RichText::new("Selected").font(body(11.5)).color(TEXT3));
                }
            },
        );
        self.tab = tab;
        match self.tab {
            Tab::Mixer => self.mixer(ui, st, f, act),
            Tab::Track => scroll_hinted(ui, egui::ScrollArea::both().auto_shrink([false, false]), |ui| self.track_tab(ui, st, f, act)),
            Tab::Master => scroll_hinted(ui, egui::ScrollArea::both().auto_shrink([false, false]), |ui| self.master_tab(ui, st, act)),
            Tab::Analysis => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.analysis_tab(ui, st));
            }
        }
    }

    /// The mixer's Bus button and its form: a name for the new bus and the tracks to route into it (the selected one
    /// to begin with).
    fn bus_button(&mut self, ui: &mut egui::Ui, p: &Project, act: &mut Vec<Action>) {
        let resp = button_ex(ui, Some(Icon::Plus), "Bus", Kind::Quiet, true).on_hover_text("Add a bus: a track other tracks play into, to mix them as one");
        if resp.clicked() {
            let tracks = self.selected.filter(|id| p.tracks.iter().any(|t| t.id == *id)).into_iter().collect();
            self.bus_form = BusForm { name: free_bus_name(p), tracks };
        }
        let form = &mut self.bus_form;
        egui::Popup::menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            const W: f32 = 240.0;
            ui.set_width(W);
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.label(RichText::new("New bus").font(semibold(12.0)).color(TEXT));
            let edit = ui.add(text_field(&mut form.name, W, "Bus name"));
            let name = form.name.trim().to_string();
            let taken = p.tracks.iter().any(|t| t.name == name);
            if taken {
                ui.label(RichText::new("A track already has that name").font(body(11.0)).color(WARN));
            }
            ui.add_space(2.0);
            caption(ui, if p.tracks.is_empty() { "No tracks to route into it yet" } else { "Tracks to route into it" });
            egui::ScrollArea::vertical().max_height(176.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for t in &p.tracks {
                    let on = form.tracks.contains(&t.id);
                    if checkbox(ui, Check::from(on), true, &t.name).clicked() {
                        if on {
                            form.tracks.retain(|x| *x != t.id);
                        } else {
                            form.tracks.push(t.id);
                        }
                    }
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let ok = !name.is_empty() && !taken;
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (primary_button(ui, None, "Add bus", ok).clicked() || enter) && ok {
                    // in track order, as the mixer shows them
                    let tracks = p.tracks.iter().map(|t| t.id).filter(|id| form.tracks.contains(id)).collect();
                    act.push(Action::AddBus(name.clone(), tracks));
                    ui.close();
                }
                if quiet_button(ui, "Cancel").clicked() {
                    ui.close();
                }
            });
        });
    }

    // -----------------------------------------------------------------------------------------
    // mixer

    fn mixer(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let stats = st.render.as_ref().filter(|r| r.revision == st.session.revision).map(|r| &r.out.tracks);
        let area = ui.available_rect_before_wrap();
        let master_r = Rect::from_min_max(Pos2::new(area.right() - MASTER_W, area.top()), area.max);
        let strips_r = Rect::from_min_max(area.min, Pos2::new(master_r.left() - 1.0, area.bottom()));
        ui.painter().rect_filled(area, 0.0, BASE);
        ui.painter().rect_filled(master_r, 0.0, PANEL);
        ui.painter().vline(px(master_r.left()) - 1.0, area.y_range(), Stroke::new(1.0, LINE));
        let mut su = ui.new_child(UiBuilder::new().max_rect(strips_r).layout(Layout::left_to_right(Align::Min)));
        scroll_hinted(&mut su, egui::ScrollArea::horizontal().id_salt("mixer_strips").auto_shrink([false, false]), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for t in &p.tracks {
                let stat = stats.and_then(|s| s.iter().find(|x| x.id == t.id));
                self.strip(ui, f, p, t, stat, strips_r.height(), act);
            }
        });
        if p.tracks.is_empty() {
            let mut eu = ui.new_child(UiBuilder::new().max_rect(strips_r.shrink2(vec2(16.0, 0.0))).layout(Layout::top_down(Align::Center)));
            eu.add_space((strips_r.height() * 0.5 - 28.0).max(16.0));
            empty_state(&mut eu, "No channel strips", "Every track gets one: level, pan, reverb send, mute and solo.");
        }
        let mut mu = ui.new_child(UiBuilder::new().max_rect(master_r).layout(Layout::top_down(Align::Min)));
        self.master_strip(&mut mu, st, master_r, act);
        ui.allocate_rect(area, Sense::hover());
    }

    /// A track's channel strip. While the project routes tracks into buses, every strip gets a row for its output and
    /// sends; a bus's strip is lighter, and tagged.
    #[allow(clippy::too_many_arguments)]
    fn strip(&mut self, ui: &mut egui::Ui, f: &Feed, p: &Project, t: &Track, stat: Option<&TrackStat>, h: f32, act: &mut Vec<Action>) {
        let (r, resp) = ui.allocate_exact_size(vec2(STRIP_W, h), Sense::click());
        if !ui.is_rect_visible(r) {
            return;
        }
        let sel = self.selected == Some(t.id);
        let bus = effects::is_bus(p, t);
        let io = p.tracks.iter().any(|x| x.mixer.output.is_some() || !x.mixer.sends.is_empty());
        let c = col(t.color);
        let pnt = ui.painter().clone();
        let fill = if bus { HEADER } else { PANEL };
        pnt.rect_filled(r, 0.0, if sel { fill.lerp_to_gamma(ACCENT, 0.12) } else { fill });
        pnt.vline(px(r.right()) - 1.0, r.y_range(), Stroke::new(1.0, LINE));
        pnt.rect_filled(Rect::from_min_size(r.min, vec2(STRIP_W - 1.0, 3.0)), 0.0, if t.mixer.mute { dimmed(c) } else { c });
        if resp.clicked() {
            self.selected = Some(t.id);
        }

        // name, then the inserts (or what the live engine is doing, when that matters)
        let text_r = Rect::from_min_max(Pos2::new(r.left() + 5.0, r.top() + 9.0), Pos2::new(r.right() - 6.0, r.top() + 42.0));
        let mut tu = ui.new_child(UiBuilder::new().max_rect(text_r).layout(Layout::top_down(Align::Center)));
        tu.spacing_mut().item_spacing.y = 1.0;
        let name_c = if t.mixer.mute { TEXT3 } else { TEXT };
        let name = if bus {
            bus_name(&mut tu, t, name_c)
        } else {
            tu.add(egui::Label::new(RichText::new(&t.name).font(medium(11.5)).color(name_c)).truncate().sense(Sense::click()))
        };
        if name.clicked() {
            self.selected = Some(t.id);
        }
        let status = f.status_of(t.id);
        match status.and_then(engine_state) {
            Some((label, lc)) => {
                tu.add(egui::Label::new(RichText::new(label).font(body(10.0)).color(lc)).truncate()).on_hover_text(status.unwrap_or_default());
            }
            None => {
                let ins = insert_names(t);
                let ins_text = if ins.is_empty() { "No inserts".to_string() } else { ins.join(" \u{b7} ") };
                tu.add(egui::Label::new(RichText::new(&ins_text).font(body(10.0)).color(TEXT3)).truncate()).on_hover_text(format!("{}\n{}", kind_label(t), ins.join("\n")));
            }
        }

        // where it goes, then pan and reverb send
        let top = if io {
            let row = Rect::from_min_size(Pos2::new(r.left() + 6.0, r.top() + 46.0), vec2(STRIP_W - 13.0, 18.0));
            if let Some(m) = strip_io(ui, p, t, row) {
                act.push(mixer_cmd(t.id, m));
            }
            r.top() + 70.0
        } else {
            r.top() + 48.0
        };
        let knobs = Rect::from_min_size(Pos2::new(r.left() + 2.0, top), vec2(80.0, 54.0));
        let mut ku = ui.new_child(UiBuilder::new().max_rect(knobs).layout(Layout::left_to_right(Align::Min)));
        ku.spacing_mut().item_spacing.x = 0.0;
        if let Some(v) = Knob::new("Pan", -1.0, 1.0, 0.0, f_pan).bipolar().size(24.0).width(40.0).show(&mut ku, Id::new(("k_pan", t.id)), t.mixer.pan) {
            act.push(mixer_cmd(t.id, MixerPatch { pan: Some(v), ..Default::default() }));
        }
        if let Some(v) = Knob::new("Verb", 0.0, 1.0, 0.0, f_pct).size(24.0).width(40.0).show(&mut ku, Id::new(("k_send", t.id)), t.mixer.reverb_send) {
            act.push(mixer_cmd(t.id, MixerPatch { reverb_send: Some(v), ..Default::default() }));
        }

        // meter and fader
        let y0 = top + 60.0;
        let y1 = (r.bottom() - 58.0).max(y0 + 60.0);
        let m = self.meters.track(t.id);
        let mr = Rect::from_min_max(Pos2::new(r.left() + 10.0, y0 + 8.0), Pos2::new(r.left() + 16.0, y1 - 8.0));
        meter_bar(&pnt, mr, true, m.level, m.hold);
        let fr = Rect::from_min_max(Pos2::new(r.left() + 20.0, y0), Pos2::new(r.right() - 10.0, y1));
        let mut fu = ui.new_child(UiBuilder::new().max_rect(fr).layout(Layout::left_to_right(Align::Min)));
        if let Some(v) = fader(&mut fu, Id::new(("fader", t.id)), t.mixer.gain_db, fr.size()) {
            act.push(mixer_cmd(t.id, MixerPatch { gain_db: Some(v), ..Default::default() }));
        }

        // gain and peak readouts
        let gb = Rect::from_min_size(Pos2::new(r.left() + 6.0, y1 + 6.0), vec2(35.0, 18.0));
        let pb = Rect::from_min_size(Pos2::new(r.left() + 43.0, y1 + 6.0), vec2(35.0, 18.0));
        value_box(ui, gb, &format!("{:.1}", t.mixer.gain_db), TEXT, FIELD);
        ui.interact(gb, Id::new(("gain_box", t.id)), Sense::hover()).on_hover_text("Gain (dB)");
        let live = m.hold > METER_FLOOR;
        let (peak_text, peak_c, peak_fill) = if live {
            let clip = m.hold >= -0.01;
            (f_level(m.hold), if clip { Color32::WHITE } else { TEXT2 }, if clip { ERR } else { FIELD })
        } else if let Some(s) = stat {
            (f_level(s.peak_dbfs), TEXT3, FIELD)
        } else {
            ("\u{2013}".into(), TEXT3, FIELD)
        };
        value_box(ui, pb, &peak_text, peak_c, peak_fill);
        let peak_tip = match (live, stat) {
            (true, _) => "Live peak (dBFS)".to_string(),
            (false, Some(s)) => {
                let loud = s.section_rms_db.iter().copied().filter(|x| x.is_finite()).fold(-120.0, f64::max);
                format!("Last render, post-fader: peak {:.1} dBFS, loudest section {loud:.1} dBFS RMS", s.peak_dbfs)
            }
            (false, None) => "Play live or render to meter this track".to_string(),
        };
        ui.interact(pb, Id::new(("peak_box", t.id)), Sense::hover()).on_hover_text(peak_tip);

        // mute, solo and a render warning
        let warn = stat.and_then(|s| s.warning.clone());
        let ms_x = if warn.is_some() { r.left() + 10.0 } else { r.center().x - 22.0 };
        let mut mu = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(ms_x.round(), r.bottom() - 28.0), vec2(70.0, 18.0))).layout(Layout::left_to_right(Align::Center)));
        mu.spacing_mut().item_spacing.x = 4.0;
        let (mute, solo) = mute_solo(&mut mu, t.id, t.mixer.mute, t.mixer.solo);
        if mute {
            act.push(mixer_cmd(t.id, MixerPatch { mute: Some(!t.mixer.mute), ..Default::default() }));
        }
        if solo {
            act.push(mixer_cmd(t.id, MixerPatch { solo: Some(!t.mixer.solo), ..Default::default() }));
        }
        if let Some(w) = warn {
            let wr = Rect::from_center_size(Pos2::new(r.right() - 18.0, r.bottom() - 19.0), vec2(14.0, 14.0));
            paint_icon(ui.painter(), wr, Icon::Warning, WARN);
            ui.interact(wr.expand(3.0), Id::new(("strip_warn", t.id)), Sense::hover()).on_hover_text(w);
        }
    }

    fn master_strip(&mut self, ui: &mut egui::Ui, st: &State, r: Rect, act: &mut Vec<Action>) {
        let m = &st.session.project.master;
        let p = ui.painter().clone();
        p.text(Pos2::new(r.left() + 12.0, r.top() + 16.0), Align2::LEFT_CENTER, "Master", semibold(12.5), TEXT);
        p.text(Pos2::new(r.right() - 12.0, r.top() + 16.0), Align2::RIGHT_CENTER, format!("Target {:.0} LUFS", m.target_lufs), body(11.0), TEXT3);

        // the output, both channels, with its scale and the held peak (click to clear)
        let mr = Rect::from_min_max(Pos2::new(r.left() + 12.0, r.top() + 38.0), Pos2::new(r.left() + 30.0, r.bottom() - 34.0));
        for (k, b) in self.meters.out.iter().enumerate() {
            let br = Rect::from_min_size(Pos2::new(mr.left() + k as f32 * 10.0, mr.top()), vec2(8.0, mr.height()));
            meter_bar(&p, br, true, b.level, b.hold);
        }
        meter_scale(&p, mr, true, &[0.0, -6.0, -10.0, -20.0, -30.0, -40.0, -60.0], mr.right() + 2.0);

        // how much the limiter took off at the last render, hanging from the top (0 to 12 dB)
        let gr = Rect::from_min_max(Pos2::new(r.left() + 55.0, mr.top()), Pos2::new(r.left() + 61.0, mr.bottom()));
        p.rect_filled(gr, 1.0, WELL);
        p.text(Pos2::new(gr.center().x, mr.top() - 8.0), Align2::CENTER_CENTER, "GR", mono(8.5), TEXT3);
        let gr_tip = match &st.render {
            Some(rr) => {
                let db = -rr.out.limiter_gr_db.min(0.0);
                let h = (gr.height() * (db.min(12.0) / 12.0) as f32).round();
                let stale = rr.revision != st.session.revision;
                p.rect_filled(Rect::from_min_size(gr.min, vec2(gr.width(), h)), 1.0, if stale { TEXT3 } else { ACCENT });
                format!("The limiter took up to {db:.1} dB off at the last render{}", if stale { " (out of date)" } else { "" })
            }
            None => "The limiter's gain reduction: render to measure it".to_string(),
        };
        ui.interact(gr.expand2(vec2(3.0, 0.0)), Id::new("master_gr"), Sense::hover()).on_hover_text(gr_tip);
        let hold = self.meters.out[0].hold.max(self.meters.out[1].hold);
        let pr = Rect::from_min_size(Pos2::new(r.left() + 8.0, r.bottom() - 26.0), vec2(44.0, 18.0));
        let clip = self.meters.clipped;
        let resp = ui.interact(pr, Id::new("master_peak"), Sense::click());
        p.rect(pr, CornerRadius::same(2), if clip { ERR } else { FIELD }, Stroke::new(1.0, if resp.hovered() { TEXT3 } else { LINE }), StrokeKind::Inside);
        p.text(pr.center(), Align2::CENTER_CENTER, f_level(hold), mono(10.0), if clip { Color32::WHITE } else { TEXT2 });
        if resp.on_hover_text("Output peak (dBFS): red once it reached 0 dBFS. Click to clear.").clicked() {
            self.meters.reset_peaks();
        }

        // what the last render measured
        let col_r = Rect::from_min_max(Pos2::new(r.left() + 70.0, r.top() + 34.0), Pos2::new(r.right() - 12.0, r.bottom() - 64.0));
        let mut cu = ui.new_child(UiBuilder::new().max_rect(col_r).layout(Layout::top_down(Align::Min)));
        cu.spacing_mut().item_spacing.y = 2.0;
        if let Some(rr) = &st.render {
            let o = &rr.out;
            cu.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(format!("{:.1}", o.lufs)).font(semibold(22.0)).color(TEXT));
                ui.label(RichText::new("LUFS").font(body(11.0)).color(TEXT3));
            });
            if rr.revision == st.session.revision {
                caption(&mut cu, "Integrated, last render");
            } else {
                cu.label(RichText::new("Render out of date").font(body(11.0)).color(WARN));
            }
            cu.add_space(6.0);
            stat_row(&mut cu, "True peak", format!("{:.2} dBTP", o.true_peak_dbtp), Some(o.true_peak_dbtp <= m.ceiling_dbtp + 0.05));
            stat_row(&mut cu, "Limiter", format!("{:.1} dB", o.limiter_gr_db), Some(o.limiter_gr_db > -6.0));
            if let Some(s) = o.seam_error_db {
                let (v, ok) = if o.seam_crossfade_ms.is_some() { ("Crossfaded".to_string(), true) } else { (format!("{s:.0} dB"), s < -60.0) };
                stat_row(&mut cu, "Loop seam", v, Some(ok));
            }
            stat_row(&mut cu, "Length", mmss(o.duration_s()), None);
        } else {
            cu.add_space(4.0);
            cu.label(RichText::new("Not rendered").font(medium(13.0)).color(TEXT2));
            cu.add(egui::Label::new(RichText::new("Render to measure loudness, peaks and the loop seam.").font(body(11.0)).color(TEXT3)).wrap());
        }

        let kr = Rect::from_min_max(Pos2::new(r.left() + 64.0, r.bottom() - 60.0), Pos2::new(r.right() - 6.0, r.bottom() - 4.0));
        let mut ku = ui.new_child(UiBuilder::new().max_rect(kr).layout(Layout::left_to_right(Align::Min)));
        ku.spacing_mut().item_spacing.x = 0.0;
        if let Some(v) = Knob::new("Target", -30.0, -6.0, -15.0, f_lufs).size(26.0).width(60.0).show(&mut ku, Id::new("k_target"), m.target_lufs) {
            act.push(master_cmd(MasterPatch { target_lufs: Some(v), ..Default::default() }));
        }
        if let Some(v) = Knob::new("Ceiling", -6.0, 0.0, -1.0, f_dbtp).size(26.0).width(60.0).show(&mut ku, Id::new("k_ceiling"), m.ceiling_dbtp) {
            act.push(master_cmd(MasterPatch { ceiling_dbtp: Some(v), ..Default::default() }));
        }
        if let Some(v) = Knob::new("Verb", -40.0, 12.0, 0.0, f_db).size(26.0).width(60.0).show(&mut ku, Id::new("k_ret"), m.reverb.return_db) {
            act.push(master_cmd(MasterPatch { reverb: Some(ReverbParams { return_db: v, ..m.reverb }), ..Default::default() }));
        }
    }

    // -----------------------------------------------------------------------------------------
    // track inspector

    fn track_tab(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let Some(t) = self.selected.and_then(|id| p.tracks.iter().find(|t| t.id == id)) else {
            ui.add_space(40.0);
            empty_state(ui, "No track selected", "Click a track in the arrangement or a strip in the mixer.");
            return;
        };
        let id = t.id;
        if ui.ctx().data(|d| d.get_temp::<u64>(Id::new("rename_for"))) != Some(id) {
            self.rename = t.name.clone();
            self.instr_path = match &t.instrument {
                Instrument::Sfz { path, .. } | Instrument::Vst3 { path, .. } | Instrument::Audio { path } => path.clone(),
                Instrument::None => String::new(),
            };
            ui.ctx().data_mut(|d| d.insert_temp(Id::new("rename_for"), id));
        }
        // the sections follow the signal: the strip's own effects, its inserts, then where it goes
        let mx = &t.mixer;
        let w = fit_widths(
            ui.available_width(),
            &[
                (204.0, 236.0),
                (232.0, 320.0),
                fixed(CHANNEL_W),
                effect_w(!mx.eq.is_empty(), effects::EQ_W),
                effect_w(mx.comp.is_some(), effects::knobs_w(5, STRIP_KNOB_W)),
                effect_w(!mx.dyn_eq.is_empty(), effects::DYN_EQ_W),
                effect_w(mx.deess.is_some(), effects::knobs_w(3, STRIP_KNOB_W)),
                effect_w(mx.reverb.is_some(), effects::knobs_w(5, STRIP_KNOB_W)),
                (248.0, INSERTS_W),
                fixed(effects::ROUTING_W),
            ],
        );
        // what the last render measured where the effects listen (their relative thresholds count from it)
        let last = st.render.as_deref();
        let stat = last.and_then(|r| r.out.tracks.iter().find(|s| s.id == id));
        let was = last.and_then(|r| r.project.tracks.iter().find(|x| x.id == id)).map(|x| &x.mixer);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let counts = if t.is_audio() {
                let n = t.clips.len();
                format!("#{id} \u{b7} {n} audio clip{} \u{b7} {} notes", if n == 1 { "" } else { "s" }, t.notes.len())
            } else {
                format!("#{id} \u{b7} {} notes \u{b7} {} CC points \u{b7} {} bends", t.notes.len(), t.ccs.len(), t.bends.len())
            };
            section(ui, w[0], "Track", &counts, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    // the rest of the row is the Rename button
                    let edit = ui.add(text_field(&mut self.rename, ui.available_width() - 76.0, "Track name"));
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let changed = self.rename.trim() != t.name && !self.rename.trim().is_empty();
                    if (button_ex(ui, None, "Rename", Kind::Secondary, changed).clicked() || enter) && changed {
                        act.push(Action::Apply(vec![Command::UpdateTrack { track: TrackRef::Id(id), name: Some(self.rename.trim().to_string()), instrument: None, color: None, layer: None }]));
                    }
                });
                ui.add_space(8.0);
                caption(ui, "Colour");
                // two rows of six
                ui.spacing_mut().item_spacing.y = 4.0;
                for row in SWATCHES.chunks(6) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for &sw in row {
                            let (r, resp) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::click());
                            let on = sw == t.color;
                            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, format!("Colour #{:02X}{:02X}{:02X}", sw[0], sw[1], sw[2])));
                            let pnt = ui.painter();
                            pnt.rect_filled(r.shrink(2.0), 2.0, col(sw));
                            if on {
                                pnt.rect_stroke(r, 3.0, Stroke::new(1.5, TEXT), StrokeKind::Inside);
                            } else if resp.hovered() {
                                pnt.rect_stroke(r, 3.0, Stroke::new(1.0, TEXT3), StrokeKind::Inside);
                            }
                            if resp.clicked() && !on {
                                act.push(Action::Apply(vec![Command::UpdateTrack { track: TrackRef::Id(id), name: None, instrument: None, color: Some(sw), layer: None }]));
                            }
                        }
                    });
                }
                ui.add_space(12.0);
                let armed_id = Id::new(("delete_armed", id));
                let armed = ui.ctx().data(|d| d.get_temp::<bool>(armed_id)).unwrap_or(false);
                let resp = danger_button(ui, if armed { "Click again to delete" } else { "Delete track" }, armed);
                if resp.clicked() {
                    if armed {
                        act.push(Action::Apply(vec![Command::RemoveTrack { track: TrackRef::Id(id) }]));
                        self.selected = None;
                    }
                    ui.ctx().data_mut(|d| d.insert_temp(armed_id, !armed));
                } else if armed && !resp.hovered() && ui.input(|i| i.pointer.any_click()) {
                    ui.ctx().data_mut(|d| d.insert_temp(armed_id, false));
                }
            });

            let kind = match &t.instrument {
                Instrument::Sfz { .. } => "SFZ sampler",
                Instrument::Vst3 { .. } => "VST3 plugin",
                Instrument::Audio { .. } => "Audio clips",
                Instrument::None => "No instrument",
            };
            section(ui, w[1], "Instrument", kind, |ui| {
                ui.add(text_field(&mut self.instr_path, ui.available_width(), "Path to an .sfz, .vst3 or audio file").font(mono(11.0)));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let has_path = !self.instr_path.trim().is_empty();
                    caption(ui, "Use as");
                    for (k, label, hint) in [
                        ("sfz", "SFZ", "Play this path as an SFZ instrument (sfizz)"),
                        ("vst3", "VST3", "Load this path as a VST3 instrument plugin (or a guitar rig)"),
                        ("audio", "Audio", "Play this audio file (WAV, FLAC or Ogg Vorbis) from the top of the song, in place of the track's clips"),
                    ] {
                        if button_ex(ui, None, label, Kind::Secondary, has_path).on_hover_text(hint).clicked() {
                            let path = self.instr_path.trim().to_string();
                            let instrument = match k {
                                "sfz" => Instrument::Sfz { path, init_cc: Default::default() },
                                "vst3" => Instrument::Vst3 { path, params: Default::default(), preset: None, preset_name: String::new(), class: None },
                                _ => Instrument::Audio { path },
                            };
                            act.push(Action::Apply(vec![Command::UpdateTrack { track: TrackRef::Id(id), name: None, instrument: Some(instrument), color: None, layer: None }]));
                        }
                    }
                });
                if let Instrument::Vst3 { path, .. } = &t.instrument
                    && !path.ends_with(".rig.json")
                {
                    ui.add_space(6.0);
                    if primary_button(ui, None, "Open plugin window", true).on_hover_text("Changes you make in the plugin's window are saved to this track (undo works)").clicked() {
                        act.push(Action::OpenPlugin(id));
                    }
                }
                if t.is_audio() {
                    ui.add_space(6.0);
                    caption(ui, "Drop audio files on the track to place more clips; drag them to move, trim and fade.");
                }
                if let Some(hint) = t.meta.get("sfz")
                    && !matches!(t.instrument, Instrument::Sfz { .. })
                {
                    ui.add_space(8.0);
                    let short = hint.rsplit(['/', '\\']).next().unwrap_or(hint);
                    caption(ui, "This stem was rendered from an SFZ. Switch to it to hear note edits.");
                    if primary_button(ui, None, &format!("Switch to {short}"), true).on_hover_text(hint).clicked() {
                        act.push(Action::Apply(vec![Command::UpdateTrack {
                            track: TrackRef::Id(id),
                            name: None,
                            instrument: Some(Instrument::Sfz { path: hint.clone(), init_cc: Default::default() }),
                            color: None,
                            layer: None,
                        }]));
                    }
                }
                if let Some(role) = t.meta.get("role") {
                    ui.add_space(6.0);
                    caption(ui, role);
                }
                if let Some(s) = f.status_of(id) {
                    ui.add_space(8.0);
                    let (label, c) = match s {
                        "no instrument" => ("No instrument", TEXT3),
                        _ => engine_state(s).unwrap_or(("Ready", OK)),
                    };
                    status_dot(ui, c, &format!("Live engine \u{b7} {label}"), TEXT2).on_hover_text(s);
                }
            });

            section(ui, w[2], "Channel strip", "Drag a knob (Shift: fine), double-click to reset", |ui| {
                let k = |ui: &mut egui::Ui, knob: Knob, key: &str, v: f64, make: &dyn Fn(f64) -> MixerPatch, act: &mut Vec<Action>| {
                    if let Some(x) = knob.show(ui, Id::new((key, id)), v) {
                        act.push(mixer_cmd(id, make(x)));
                    }
                };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    k(ui, Knob::new("Gain", -48.0, 24.0, 0.0, f_db), "t_gain", mx.gain_db, &|v| MixerPatch { gain_db: Some(v), ..Default::default() }, act);
                    k(ui, Knob::new("Pan", -1.0, 1.0, 0.0, f_pan).bipolar(), "t_pan", mx.pan, &|v| MixerPatch { pan: Some(v), ..Default::default() }, act);
                    k(ui, Knob::new("Width", 0.0, 2.0, 1.0, f_pct), "t_width", mx.width, &|v| MixerPatch { width: Some(v), ..Default::default() }, act);
                    k(ui, Knob::new("Low cut", 20.0, 2000.0, 0.0, f_hz).scale(Scale::LogOff), "t_hpf", mx.hpf_hz, &|v| MixerPatch { hpf_hz: Some(v), ..Default::default() }, act);
                    k(ui, Knob::new("High cut", 200.0, 20000.0, 0.0, f_hz).scale(Scale::LogOff), "t_lpf", mx.lpf_hz, &|v| MixerPatch { lpf_hz: Some(v), ..Default::default() }, act);
                    k(ui, Knob::new("Verb", 0.0, 1.0, 0.0, f_pct), "t_send", mx.reverb_send, &|v| MixerPatch { reverb_send: Some(v), ..Default::default() }, act);
                });
                ui.add_space(2.0);
                // each cut's slope, under its knob (past Gain, Pan and Width)
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.add_space(3.0 * (STRIP_KNOB_W + 4.0));
                    if let Some(s) = slope_menu(ui, mx.hpf_slope, mx.hpf_hz > 0.0, "low") {
                        act.push(mixer_cmd(id, MixerPatch { hpf_slope: Some(s), ..Default::default() }));
                    }
                    if let Some(s) = slope_menu(ui, mx.lpf_slope, mx.lpf_hz > 0.0, "high") {
                        act.push(mixer_cmd(id, MixerPatch { lpf_slope: Some(s), ..Default::default() }));
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    k(ui, Knob::new("Drive", 0.0, 36.0, 0.0, f_db), "t_drv", mx.drive_db, &|v| MixerPatch { drive_db: Some(v), ..Default::default() }, act);
                    k(ui, Knob::new("Post", -48.0, 12.0, 0.0, f_db), "t_post", mx.post_gain_db, &|v| MixerPatch { post_gain_db: Some(v), ..Default::default() }, act);
                    k(ui, Knob::new("Crush", 0.0, 16.0, 0.0, f_bits), "t_bits", mx.bitcrush_bits, &|v| MixerPatch { bitcrush_bits: Some(if v < 1.0 { 0.0 } else { v }), ..Default::default() }, act);
                    k(ui, Knob::new("Mono <", 20.0, 500.0, 0.0, f_hz).scale(Scale::LogOff), "t_mono", mx.mono_below_hz, &|v| MixerPatch { mono_below_hz: Some(v), ..Default::default() }, act);
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.add_space(4.0);
                        let mut chorus = mx.chorus.is_some();
                        if switch(ui, &mut chorus, "Chorus") {
                            let v = chorus.then_some(ChorusFx { rate_hz: 0.5, depth: 0.3, mix: 0.4, centre_delay_ms: 7.0 });
                            act.push(mixer_cmd(id, MixerPatch { chorus: Some(v), ..Default::default() }));
                        }
                        let others: Vec<&str> = [
                            mx.gate.is_some().then_some("gate"),
                            mx.delay.is_some().then_some("delay"),
                            mx.duck.is_some().then_some("sidechain duck"),
                            mx.clip_db.is_some().then_some("clipper"),
                            mx.amp.is_some().then_some("amp"),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();
                        if !others.is_empty() {
                            caption(ui, &format!("Also on: {} (set by the agent)", others.join(", ")));
                        }
                    });
                });
            });

            let plural = |n: usize| if n == 1 { "" } else { "s" };
            let n = mx.eq.len();
            let sub = if n == 0 { "After the low and high cuts".to_string() } else { format!("{n} band{}, after the cuts", plural(n)) };
            section(ui, w[3], "EQ", &sub, |ui| {
                if let Some(v) = effects::eq(ui, &format!("t_eq_{id}"), "EQ", &mx.eq) {
                    act.push(mixer_cmd(id, MixerPatch { eq: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[4], "Compressor", "Evens out the level", |ui| {
                let fresh = Comp { threshold_db: -6.0, relative: true, ratio: 3.0, attack_ms: 10.0, release_ms: 120.0, ..Comp::default() };
                if let Some(v) = effects::comp(ui, &format!("t_comp_{id}"), mx.comp, fresh, STRIP_KNOB_W, stat.and_then(|s| s.comp_loud_db)) {
                    act.push(mixer_cmd(id, MixerPatch { comp: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[5], "Dynamic EQ", "Turns bands down when loud", |ui| {
                // a band's level counts while it listens where it did at the render
                let loud: Vec<Option<f64>> = mx
                    .dyn_eq
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        let same = was.and_then(|m| m.dyn_eq.get(i)).is_some_and(|r| r.kind == b.kind && r.hz == b.hz && r.q == b.q);
                        stat.filter(|_| same).and_then(|s| s.dyn_eq_loud_db.get(i).copied())
                    })
                    .collect();
                if let Some(v) = effects::dyn_eq(ui, &format!("t_dyn_{id}"), &mx.dyn_eq, &loud) {
                    act.push(mixer_cmd(id, MixerPatch { dyn_eq: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[6], "De-esser", "Tames harsh highs", |ui| {
                let same = was.and_then(|m| m.deess).map(|d| d.hz) == mx.deess.map(|d| d.hz);
                let loud = stat.filter(|_| same).and_then(|s| s.deess_loud_db);
                if let Some(v) = effects::deess(ui, &format!("t_deess_{id}"), mx.deess, STRIP_KNOB_W, loud) {
                    act.push(mixer_cmd(id, MixerPatch { deess: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[7], "Reverb", "The track's own reverb", |ui| {
                // an aux return is all reverb; on a track playing itself, some
                let fresh = ReverbFx { mix: if effects::aux_return(p, t) { 1.0 } else { 0.25 }, ..ReverbFx::default() };
                if let Some(v) = effects::strip_reverb(ui, &format!("t_verb_{id}"), mx.reverb, fresh, STRIP_KNOB_W) {
                    act.push(mixer_cmd(id, MixerPatch { reverb: Some(v), ..Default::default() }));
                }
            });
            let n = mx.inserts.len();
            let sub = if n == 0 { "VST3 effects on this track".to_string() } else { format!("{n} VST3 effect{}, in order", plural(n)) };
            section(ui, w[8], "Inserts", &sub, |ui| inserts_ui(ui, p, Chain::Track(id), act));
            section(ui, w[9], "Routing", "Output and sends", |ui| {
                if let Some(m) = effects::routing(ui, &format!("t_route_{id}"), p, t, w[9]) {
                    act.push(mixer_cmd(id, m));
                }
            });
        });
    }

    // -----------------------------------------------------------------------------------------
    // master

    fn master_tab(&mut self, ui: &mut egui::Ui, st: &State, act: &mut Vec<Action>) {
        // the knob sections are as wide as their rows of knobs
        const KNOB_W: f32 = 56.0;
        let row_w = |n: usize| effects::knobs_w(n, KNOB_W);
        let p = &st.session.project;
        let m = &p.master;
        let last = st.render.as_deref();
        let mk = |ui: &mut egui::Ui, knob: Knob, key: &str, v: f64, make: &dyn Fn(f64) -> MasterPatch, act: &mut Vec<Action>| {
            if let Some(x) = knob.width(KNOB_W).show(ui, Id::new(key), v) {
                act.push(master_cmd(make(x)));
            }
        };
        // the loud levels the last render measured, for the stages set as they were then
        let was = last.map(|r| &r.project.master);
        let loud = last.map(|r| &r.out.master_loud);
        let dyn_loud: Vec<Option<f64>> = (m.dyn_eq.iter().enumerate())
            .map(|(i, b)| {
                let same = was.and_then(|w| w.dyn_eq.get(i)).is_some_and(|r| r.kind == b.kind && r.hz == b.hz && r.q == b.q);
                loud.filter(|_| same).and_then(|l| l.dyn_eq.get(i).copied())
            })
            .collect();
        let deess_loud = loud.filter(|_| was.and_then(|w| w.deess).map(|d| d.hz) == m.deess.map(|d| d.hz)).and_then(|l| l.deess);
        let split_as_rendered = |now: Option<&Vec<f64>>, then: Option<&Vec<f64>>, levels: &[f64]| -> Vec<Option<f64>> {
            match (now, loud) {
                (Some(x), Some(_)) if then == Some(x) => (0..=x.len()).map(|i| levels.get(i).copied()).collect(),
                _ => vec![],
            }
        };
        let mb_loud = split_as_rendered(
            m.multiband.as_ref().map(|x| &x.crossovers_hz),
            was.and_then(|w| w.multiband.as_ref()).map(|x| &x.crossovers_hz),
            loud.map_or(&[][..], |l| &l.multiband),
        );
        let ex_loud = split_as_rendered(
            m.exciter.as_ref().map(|x| &x.crossovers_hz),
            was.and_then(|w| w.exciter.as_ref()).map(|x| &x.crossovers_hz),
            loud.map_or(&[][..], |l| &l.exciter),
        );
        // the loudness and its limiter first, then the chain in the order it runs, the reverb and the inserts
        let w = fit_widths(
            ui.available_width(),
            &[
                fixed(row_w(4)),
                effect_w(m.glue.is_some(), row_w(5)),
                effect_w(!m.eq.is_empty(), effects::EQ_W),
                effect_w(!m.dyn_eq.is_empty(), effects::DYN_EQ_W),
                effect_w(m.deess.is_some(), row_w(3)),
                effect_w(m.multiband.is_some(), effects::MULTIBAND_W),
                effect_w(m.exciter.is_some(), effects::EXCITER_W),
                fixed(effects::IMAGER_W),
                fixed(effects::OFF_W),
                fixed(row_w(4)),
                (248.0, INSERTS_W),
                (248.0, INSERTS_W),
            ],
        );
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            section(ui, w[0], "Loudness", "The mix is driven to the target automatically", |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    mk(ui, Knob::new("Target", -30.0, -6.0, -15.0, f_lufs), "m_target", m.target_lufs, &|v| MasterPatch { target_lufs: Some(v), ..Default::default() }, act);
                    mk(ui, Knob::new("Ceiling", -6.0, 0.0, -1.0, f_dbtp), "m_ceiling", m.ceiling_dbtp, &|v| MasterPatch { ceiling_dbtp: Some(v), ..Default::default() }, act);
                    mk(ui, Knob::new("Tail", 0.0, 30.0, 6.0, f_s), "m_tail", m.tail_s, &|v| MasterPatch { tail_s: Some(v), ..Default::default() }, act);
                    mk(ui, Knob::new("Edge fade", 1.0, 2000.0, 6.0, f_ms).scale(Scale::Log), "m_fade", m.edge_fade_ms.max(1.0), &|v| MasterPatch { edge_fade_ms: Some(v), ..Default::default() }, act);
                });
                ui.add_space(14.0);
                ui.label(RichText::new("Limiter").font(semibold(12.0)).color(TEXT));
                ui.label(RichText::new("Holds the true peak under the ceiling").font(body(11.0)).color(TEXT3));
                ui.add_space(8.0);
                if let Some(l) = effects::limiter(ui, "m_limiter", m.limiter, KNOB_W) {
                    act.push(master_cmd(MasterPatch { limiter: Some(l), ..Default::default() }));
                }
                ui.add_space(4.0);
                let gr = match last {
                    Some(r) => format!("It took up to {:.1} dB off at the last render", -r.out.limiter_gr_db.min(0.0)),
                    None => "Render to see how much it takes off".to_string(),
                };
                caption(ui, &gr);
            });
            section(ui, w[1], "Glue compressor", "Holds the mix together", |ui| {
                let fresh = Comp { threshold_db: -3.0, relative: true, ratio: 1.8, attack_ms: 30.0, release_ms: 250.0, ..Comp::default() };
                if let Some(g) = effects::comp(ui, "m_glue", m.glue, fresh, KNOB_W, last.and_then(|r| r.out.glue_loud_db)) {
                    act.push(master_cmd(MasterPatch { glue: Some(g), ..Default::default() }));
                }
            });
            let n = m.eq.len();
            let sub = if n == 0 { "After the glue".to_string() } else { format!("{n} band{}, after the glue", if n == 1 { "" } else { "s" }) };
            section(ui, w[2], "EQ", &sub, |ui| {
                let mut linear = m.eq_phase == EqPhase::Linear;
                let flip = ui.horizontal(|ui| switch(ui, &mut linear, "Linear phase")).inner;
                if flip {
                    let phase = if linear { EqPhase::Linear } else { EqPhase::Minimum };
                    act.push(master_cmd(MasterPatch { eq_phase: Some(phase), ..Default::default() }));
                }
                ui.add_space(6.0);
                if let Some(v) = effects::eq(ui, "m_eq", "Master EQ", &m.eq) {
                    act.push(master_cmd(MasterPatch { eq: Some(v), ..Default::default() }));
                }
            });
            let n = m.dyn_eq.len();
            let sub = if n == 0 { "Cuts a range when it's loud".to_string() } else { format!("{n} band{}, after the EQ", if n == 1 { "" } else { "s" }) };
            section(ui, w[3], "Dynamic EQ", &sub, |ui| {
                if let Some(v) = effects::dyn_eq(ui, "m_dyn", &m.dyn_eq, &dyn_loud) {
                    act.push(master_cmd(MasterPatch { dyn_eq: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[4], "De-esser", "Tames bursts of sibilance", |ui| {
                if let Some(v) = effects::deess(ui, "m_deess", m.deess, KNOB_W, deess_loud) {
                    act.push(master_cmd(MasterPatch { deess: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[5], "Multiband", "Compresses each band apart", |ui| {
                if let Some(v) = effects::multiband(ui, "m_multiband", m.multiband.as_ref(), &mb_loud) {
                    act.push(master_cmd(MasterPatch { multiband: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[6], "Exciter", "Harmonics for a dull or thin band", |ui| {
                if let Some(v) = effects::exciter(ui, "m_exciter", m.exciter.as_ref(), &ex_loud) {
                    act.push(master_cmd(MasterPatch { exciter: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[7], "Imager", "Each band's width", |ui| {
                if let Some(v) = effects::imager(ui, "m_imager", m.imager.as_ref()) {
                    act.push(master_cmd(MasterPatch { imager: Some(v), ..Default::default() }));
                }
            });
            section(ui, w[8], "Image", "Stereo, before the limiter", |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    mk(ui, Knob::new("Mono below", 20.0, 500.0, 0.0, f_hz).scale(Scale::LogOff), "m_mono", m.mono_below_hz, &|v| MasterPatch { mono_below_hz: Some(v), ..Default::default() }, act);
                    mk(ui, Knob::new("Width", 0.0, 2.0, 1.0, f_pct), "m_width", m.width, &|v| MasterPatch { width: Some(v), ..Default::default() }, act);
                });
            });
            section(ui, w[9], "Shared reverb", "Every track's Verb knob sends here", |ui| {
                let rv = m.reverb;
                let mut out = effects::algorithm(ui, "m_reverb", rv);
                for row in [
                    [(effects::Rv::Size, "Room"), (effects::Rv::Decay, "Decay"), (effects::Rv::Predelay, "Pre-delay"), (effects::Rv::Damping, "Damping")],
                    [(effects::Rv::Width, "Width"), (effects::Rv::LowCut, "Low cut"), (effects::Rv::HighCut, "High cut"), (effects::Rv::Level, "Return")],
                ] {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for (which, label) in row {
                            out = effects::reverb_knob(ui, "m_reverb", rv, which, label, KNOB_W).or(out);
                        }
                    });
                }
                if let Some(r) = out {
                    act.push(master_cmd(MasterPatch { reverb: Some(r), ..Default::default() }));
                }
                if !m.reverb_inserts.is_empty() {
                    ui.add_space(4.0);
                    caption(ui, "Replaced by the reverb return's inserts (Return still sets their level)");
                }
            });
            section(ui, w[10], "Reverb return", "Effects here replace the shared reverb", |ui| inserts_ui(ui, p, Chain::Reverb, act));
            section(ui, w[11], "Master inserts", "After the reverb joins, before glue", |ui| inserts_ui(ui, p, Chain::Master, act));
        });
    }

    // -----------------------------------------------------------------------------------------
    // analysis

    /// The master check of render `r` the analysis shows: the agent's last check of it, unless a style was picked
    /// here, else one run against the style picked. None while it runs.
    fn master_report(&mut self, ctx: &egui::Context, r: &Arc<LastRender>) -> Option<Checked> {
        let v = &mut self.master_view;
        if v.style.is_none()
            && let Some(rep) = crate::measure::checked(r)
        {
            return Some(Ok(rep));
        }
        let want = (r.seq, v.style);
        if let Some((seq, style, rep)) = &v.shown
            && (*seq, *style) == want
        {
            return Some(rep.clone());
        }
        let style = v.style.unwrap_or_default();
        let done = |x: anyhow::Result<MasterReport>| x.map(Arc::new).map_err(|e| format!("{e:#}"));
        let rep = if v.inline {
            done(crate::measure::check_for_gui(r, style))
        } else {
            match v.running.take() {
                Some((seq, s, h)) if (seq, s) == want && h.is_finished() => match h.join() {
                    Ok(x) => done(x),
                    Err(_) => Err("the check stopped unexpectedly (a bug in Flux)".to_string()),
                },
                running => {
                    // a check of another render or style is left to finish on its own
                    v.running = match running {
                        Some(job) if (job.0, job.1) == want => Some(job),
                        _ => {
                            let r = r.clone();
                            match std::thread::Builder::new().name("master-check".into()).spawn(move || crate::measure::check_for_gui(&r, style)) {
                                Ok(h) => Some((want.0, want.1, h)),
                                Err(e) => return Some(Err(format!("couldn't start the check: {e}"))),
                            }
                        }
                    };
                    ctx.request_repaint_after(Duration::from_millis(100));
                    return None;
                }
            }
        };
        v.shown = Some((want.0, want.1, rep.clone()));
        Some(rep)
    }

    /// The master check, as the agent's `master` tool makes it: the tonal balance against the target, each band's
    /// stereo image, and the findings with the score.
    fn master_check_ui(&mut self, ui: &mut egui::Ui, r: &Arc<LastRender>, pad: Margin) {
        const PLOT_H: f32 = 132.0;
        let rep = self.master_report(ui.ctx(), r);
        let ok = rep.as_ref().and_then(|x| x.as_ref().ok()).cloned();
        let w = ui.available_width();
        let tonal_w = (w * 0.4).floor().max(380.0);
        let stereo_w = 300.0;
        let findings_w = (w - tonal_w - stereo_w - 2.0).max(320.0);
        let mut pick = None;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            analysis_panel(ui, tonal_w, pad, |ui| {
                panel_head(ui, "Tonal balance", |ui| {
                    let now = self.master_view.style.or(ok.as_ref().map(|x| x.style)).unwrap_or_default();
                    let name = style_name(now);
                    let resp = chip(ui, 104.0, name, true, &format!("Target style, {name}")).on_hover_text("Which masters the balance is held against");
                    pick = menu_pick(&resp, now, &STYLES, 220.0);
                });
                let Some(rep) = &ok else { return check_pending(ui, &rep) };
                let Some(t) = &rep.tonal else {
                    caption(ui, "Too quiet to measure");
                    return;
                };
                let against = if t.target == "reference" { "the reference".to_string() } else { format!("the {} masters", t.target) };
                caption(ui, &format!("1/3 octaves against {against}, at equal loudness"));
                ui.add_space(8.0);
                tonal_plot(ui, t, PLOT_H);
                ui.add_space(4.0);
                let closest = t.closest_style.filter(|s| s.name() != t.target).map(|s| format!(" \u{b7} closest to {}", style_name(s).to_lowercase())).unwrap_or_default();
                caption(ui, &format!("{:.1} dB RMS from the target{closest}", t.deviation_rms_db));
            });
            panel_sep(ui, 214.0);
            analysis_panel(ui, stereo_w, pad, |ui| {
                panel_head(ui, "Stereo", |_| {});
                let Some(rep) = &ok else { return check_pending(ui, &rep) };
                caption(ui, "Correlation per band: 1 is mono, under 0 cancels");
                ui.add_space(8.0);
                let s = &rep.stereo;
                for b in &s.bands {
                    let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
                    let p = ui.painter();
                    let name = match b.band {
                        "low" => "Low",
                        "low_mid" => "Low mid",
                        "mid" => "Mid",
                        _ => "High",
                    };
                    p.text(Pos2::new(row.left(), row.center().y), Align2::LEFT_CENTER, name, medium(11.0), TEXT2);
                    let bar = Rect::from_min_max(Pos2::new(row.left() + 64.0, row.top() + 3.0), Pos2::new(row.right() - 44.0, row.bottom() - 3.0));
                    match b.correlation {
                        Some(c) => {
                            correlation_bar(p, bar, c, phase_limits(b.band));
                            p.text(Pos2::new(row.right(), row.center().y), Align2::RIGHT_CENTER, format!("{c:.2}"), mono(10.5), TEXT);
                            let side = b.side_mid_db.map(|x| format!(", its side {:.1} dB under its mid", -x)).unwrap_or_default();
                            resp.on_hover_text(format!("{} to {}: correlation {c:.2}{side}", hz_words(b.lo_hz), hz_words(b.hi_hz)));
                        }
                        None => {
                            p.rect_filled(bar, 2.0, FIELD);
                            p.text(Pos2::new(row.right(), row.center().y), Align2::RIGHT_CENTER, "\u{2013}", mono(10.5), TEXT3);
                            resp.on_hover_text("Too quiet to measure");
                        }
                    }
                }
                ui.add_space(6.0);
                let mut facts = vec![];
                if let Some(c) = s.correlation {
                    facts.push(format!("Whole master {c:.2}"));
                }
                if let Some(d) = s.fold_down_lu {
                    // + 0.0 turns a -0.0 into 0.0
                    facts.push(format!("mono fold-down {:+.1} LU", (d * 10.0).round() / 10.0 + 0.0));
                }
                ui.add(egui::Label::new(RichText::new(facts.join(" \u{b7} ")).font(body(11.0)).color(TEXT3)).wrap());
            });
            panel_sep(ui, 214.0);
            analysis_panel(ui, findings_w, pad, |ui| {
                panel_head(ui, "Findings", |ui| {
                    if let Some(rep) = &ok {
                        ui.label(RichText::new(format!("Score {} of 100", rep.score)).font(semibold(12.0)).color(TEXT))
                            .on_hover_text("100 is a master with nothing to fix; each finding takes off points by how much it matters");
                    }
                });
                let Some(rep) = &ok else { return check_pending(ui, &rep) };
                let l = &rep.loudness;
                let fig = |v: Option<f64>, unit: &str| v.map_or("\u{2013}".to_string(), |x| format!("{x:.1} {unit}"));
                caption(ui, &format!("LRA {} \u{b7} PLR {} \u{b7} loudest 3 s {}", fig(l.lra_lu, "LU"), fig(l.plr_db, "dB"), fig(l.short_term_max_lufs, "LUFS")));
                ui.add_space(8.0);
                if rep.issues.is_empty() {
                    caption(ui, "Nothing to fix");
                    return;
                }
                egui::ScrollArea::vertical().id_salt("findings").max_height(PLOT_H + 16.0).auto_shrink([false, true]).show(ui, |ui| {
                    for i in &rep.issues {
                        let dot = match i.severity {
                            "problem" => ERR,
                            "warn" => WARN,
                            _ => TEXT3,
                        };
                        let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
                        let g = one_line(ui, &i.message, body(11.0), TEXT2, row.width() - 14.0);
                        let p = ui.painter();
                        p.circle_filled(Pos2::new(row.left() + 4.0, row.center().y), 3.0, dot);
                        p.galley(Pos2::new(row.left() + 14.0, row.center().y - g.size().y * 0.5), g, TEXT2);
                        let fix = i.suggestion.as_ref().map(|m| format!("\n\nThe move: {}", move_text(m))).unwrap_or_default();
                        resp.on_hover_text(format!("{}{fix}", i.message));
                    }
                });
            });
        });
        if let Some(s) = pick {
            self.master_view.style = Some(s);
        }
    }

    fn analysis_tab(&mut self, ui: &mut egui::Ui, st: &State) {
        let Some(r) = st.render.clone() else {
            ui.add_space(40.0);
            empty_state(ui, "Nothing to measure yet", "Render (Ctrl+R) to see loudness, spectrum and track balance.");
            return;
        };
        if self.analysis.as_ref().map(|a| a.seq) != Some(r.seq) {
            let bars = flux_engine::analysis::bars(&r.out).as_array().map(|v| v.iter().map(|x| x["rms_db"].as_f64().unwrap_or(-120.0)).collect()).unwrap_or_default();
            let bands_v = flux_engine::analysis::bands(&r.out);
            let mut bands: Vec<(String, f64)> = bands_v["bands"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(-60.0))).collect()).unwrap_or_default();
            bands.sort_by_key(|b| b.0.trim_end_matches(" Hz").parse::<u32>().unwrap_or(0));
            self.analysis = Some(AnalysisCache { seq: r.seq, bars, bands });
        }
        let a = self.analysis.as_ref().unwrap();
        let o = &r.out;
        let m = &r.project.master;
        let stale = r.revision != st.session.revision;
        let pad = Margin { left: 16, right: 16, top: 12, bottom: 12 };

        // key figures, separated by hairlines
        let (strip, _) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::hover());
        ui.painter().hline(strip.x_range(), px(strip.bottom()) - 1.0, Stroke::new(1.0, LINE_SOFT));
        let mut su = ui.new_child(UiBuilder::new().max_rect(strip).layout(Layout::left_to_right(Align::Min)));
        su.spacing_mut().item_spacing.x = 0.0;
        let tile = |ui: &mut egui::Ui, label: &str, value: String, ok: Option<bool>| {
            let (tr, _) = ui.allocate_exact_size(vec2(132.0, strip.height()), Sense::hover());
            let p = ui.painter();
            let mut x = tr.left() + 16.0;
            if let Some(ok) = ok {
                p.circle_filled(Pos2::new(x + 3.0, tr.top() + 18.0), 3.0, if ok { OK } else { WARN });
                x += 12.0;
            }
            p.text(Pos2::new(x, tr.top() + 18.0), Align2::LEFT_CENTER, label, body(11.0), TEXT2);
            p.text(Pos2::new(tr.left() + 16.0, tr.top() + 38.0), Align2::LEFT_CENTER, value, semibold(17.0), TEXT);
            p.vline(px(tr.right()) - 1.0, tr.y_range().shrink(10.0), Stroke::new(1.0, LINE_SOFT));
        };
        tile(&mut su, "Integrated", format!("{:.1} LUFS", o.lufs), Some((o.lufs - m.target_lufs).abs() <= 0.5));
        tile(&mut su, "True peak", format!("{:.2} dBTP", o.true_peak_dbtp), Some(o.true_peak_dbtp <= m.ceiling_dbtp + 0.05));
        tile(&mut su, "Limiter", format!("{:.1} dB", o.limiter_gr_db), Some(o.limiter_gr_db > -6.0));
        if let Some(s) = o.seam_error_db {
            let (v, ok) = match o.seam_crossfade_ms {
                Some(ms) => (format!("{ms:.0} ms fade"), true),
                None => (format!("{s:.0} dB"), s < -60.0),
            };
            tile(&mut su, "Loop seam", v, Some(ok));
        }
        tile(&mut su, "Length", mmss(o.duration_s()), None);
        su.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(16.0);
            if stale {
                status_dot(ui, WARN, &format!("Render of revision {}: the project changed since", r.revision), TEXT2);
            } else {
                status_dot(ui, OK, &format!("Render of revision {} \u{b7} {:.1} s to render", r.revision, o.render_seconds), TEXT2);
            }
        });

        // per-bar loudness and the spectrum, side by side
        let half = ((ui.available_width() - 1.0) * 0.5).floor();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            egui::Frame::new().inner_margin(pad).show(ui, |ui| {
                ui.set_width(half - 32.0);
                ui.vertical(|ui| {
                    title(ui, "Loudness per bar");
                    caption(ui, "RMS dBFS, coloured by section");
                    ui.add_space(8.0);
                    let (full, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 116.0), Sense::hover());
                    let rect = full.with_min_x(full.left() + 26.0);
                    let pnt = ui.painter();
                    pnt.rect_filled(rect, 2.0, FIELD);
                    let n = a.bars.len().max(1);
                    let bw = rect.width() / n as f32;
                    // scale to the song's own range so the dynamics read
                    let audible: Vec<f64> = a.bars.iter().copied().filter(|d| *d > -90.0).collect();
                    let hi = (audible.iter().copied().fold(-90.0, f64::max) / 3.0).ceil() * 3.0 + 3.0;
                    let lo = (audible.iter().copied().fold(0.0, f64::min).max(hi - 48.0) / 6.0).floor() * 6.0 - 6.0;
                    let y_of = |db: f64| rect.bottom() - (((db - lo) / (hi - lo)).clamp(0.0, 1.0) as f32) * rect.height();
                    let mut g = (hi / 6.0).floor() * 6.0;
                    while g > lo {
                        let y = px(y_of(g));
                        pnt.hline(rect.x_range(), y, Stroke::new(1.0, LINE_SOFT));
                        pnt.text(Pos2::new(rect.left() - 5.0, y), Align2::RIGHT_CENTER, format!("{g:.0}"), mono(9.0), TEXT3);
                        g -= 6.0;
                    }
                    for (i, db) in a.bars.iter().enumerate() {
                        let bar = i as u32 + 1;
                        let si = r.project.sections.iter().position(|s| s.start_bar <= bar && bar < s.start_bar + s.bars).unwrap_or(0);
                        let c = FIELD.lerp_to_gamma(section_color(si), 0.7);
                        let x0 = rect.left() + i as f32 * bw;
                        pnt.rect_filled(Rect::from_min_max(Pos2::new(x0 + 0.5, y_of(*db)), Pos2::new(x0 + bw - 0.5, rect.bottom())), 0.0, c);
                    }
                    if let Some(h) = resp.hover_pos() {
                        let i = (((h.x - rect.left()).max(0.0) / bw) as usize).min(n - 1);
                        let sec = r.project.section_at(i as u32 + 1).map(|s| format!(" ({})", s.name)).unwrap_or_default();
                        resp.on_hover_text(format!("Bar {}{sec}: {:.1} dB", i + 1, a.bars.get(i).copied().unwrap_or(f64::NAN)));
                    }
                });
            });
            let (sep, _) = ui.allocate_exact_size(vec2(1.0, 172.0), Sense::hover());
            ui.painter().vline(px(sep.left()), sep.y_range(), Stroke::new(1.0, LINE_SOFT));
            egui::Frame::new().inner_margin(pad).show(ui, |ui| {
                ui.set_width(half - 32.0);
                ui.vertical(|ui| {
                    title(ui, "Spectrum balance");
                    caption(ui, "Octave bands, dB relative to the total");
                    ui.add_space(8.0);
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 116.0), Sense::hover());
                    let pnt = ui.painter();
                    pnt.rect_filled(Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.bottom() - 16.0)), 2.0, FIELD);
                    let n = a.bands.len().max(1);
                    let bw = rect.width() / n as f32;
                    for (i, (name, db)) in a.bands.iter().enumerate() {
                        let h = (((db + 40.0) / 40.0).clamp(0.0, 1.0) as f32) * (rect.height() - 32.0);
                        let x0 = rect.left() + i as f32 * bw;
                        let bar = Rect::from_min_max(Pos2::new(x0 + 6.0, rect.bottom() - 16.0 - h), Pos2::new(x0 + bw - 6.0, rect.bottom() - 16.0));
                        pnt.rect_filled(bar, CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 }, ACCENT.lerp_to_gamma(FIELD, 0.3));
                        pnt.text(Pos2::new(x0 + bw * 0.5, bar.top() - 2.0), Align2::CENTER_BOTTOM, format!("{db:.0}"), mono(9.0), TEXT2);
                        let label = name.trim_end_matches(" Hz").replace("000", "k");
                        pnt.text(Pos2::new(x0 + bw * 0.5, rect.bottom()), Align2::CENTER_BOTTOM, label, body(10.0), TEXT3);
                    }
                });
            });
        });
        ui.painter().hline(ui.max_rect().x_range(), px(ui.cursor().top()) - 1.0, Stroke::new(1.0, LINE_SOFT));

        self.master_check_ui(ui, &r, pad);
        ui.painter().hline(ui.max_rect().x_range(), px(ui.cursor().top()) - 1.0, Stroke::new(1.0, LINE_SOFT));

        // per-track loudness per section
        egui::Frame::new().inner_margin(pad).show(ui, |ui| {
            ui.set_width(ui.available_width());
            title(ui, "Track balance");
            caption(ui, "Post-fader RMS dBFS per section (\u{b7} = silent)");
            ui.add_space(8.0);
            egui::ScrollArea::horizontal().id_salt("balance_scroll").show(ui, |ui| {
                egui::Grid::new("balance").spacing([2.0, 2.0]).show(ui, |ui| {
                    ui.label("");
                    for (i, s) in o.sections.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            let (dr, _) = ui.allocate_exact_size(vec2(3.0, 12.0), Sense::hover());
                            ui.painter().rect_filled(dr, 0.0, section_color(i));
                            ui.add(egui::Label::new(RichText::new(&s.name).font(medium(10.5)).color(TEXT2)).truncate()).on_hover_text(&s.name);
                        });
                    }
                    ui.end_row();
                    for t in &o.tracks {
                        let tc = r.project.tracks.iter().find(|x| x.id == t.id).map(|x| col(x.color)).unwrap_or(TEXT3);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let (dr, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
                            ui.painter().rect_filled(Rect::from_center_size(dr.center(), vec2(8.0, 8.0)), 2.0, tc);
                            ui.label(RichText::new(&t.name).font(body(11.5)).color(TEXT));
                            ui.add_space(8.0);
                        });
                        for v in &t.section_rms_db {
                            let (cr, resp) = ui.allocate_exact_size(vec2(84.0, 22.0), Sense::hover());
                            if *v < -100.0 {
                                ui.painter().rect_filled(cr, 2.0, FIELD);
                                ui.painter().text(cr.center(), Align2::CENTER_CENTER, "\u{b7}", mono(11.0), TEXT3);
                            } else {
                                let k = ((v + 50.0) / 40.0).clamp(0.0, 1.0) as f32;
                                let fill = FIELD.lerp_to_gamma(ACCENT, 0.12 + 0.68 * k);
                                ui.painter().rect_filled(cr, 2.0, fill);
                                ui.painter().text(cr.center(), Align2::CENTER_CENTER, format!("{v:.0}"), mono(10.5), if k > 0.55 { Color32::WHITE } else { TEXT });
                                resp.on_hover_text(format!("{}: {v:.1} dBFS", t.name));
                            }
                        }
                        ui.end_row();
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_give_up_width_or_keep_it_and_scroll() {
        let sections = [(100.0, 200.0), fixed(300.0)];
        // room for every section (each with its 32 px of padding)
        assert_eq!(fit_widths(1000.0, &sections), vec![200.0, 300.0]);
        // 44 px short: the flexible one gives them up
        assert_eq!(fit_widths(520.0, &sections), vec![156.0, 300.0]);
        // shorter than the narrowest: full widths, and the tab scrolls
        assert_eq!(fit_widths(400.0, &sections), vec![200.0, 300.0]);
        assert_eq!(effect_w(false, 400.0), fixed(effects::OFF_W));
        assert_eq!(effect_w(true, 400.0), fixed(400.0));
    }

    #[test]
    fn new_buses_get_free_names() {
        let mut p = Project::new("t", 120.0, 4);
        assert_eq!(free_bus_name(&p), "Bus 1");
        for n in ["Bus 1", "Bus 2"] {
            flux_core::commands::apply(&mut p, &Command::AddTrack { name: n.into(), instrument: Instrument::None, color: None }).unwrap();
        }
        assert_eq!(free_bus_name(&p), "Bus 3");
    }
}
