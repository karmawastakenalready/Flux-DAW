//! Instrument report cards: render an SFZ through sfizz, measure it, and describe it in numbers.
//!
//! The composer can't listen, so every instrument is measured before it is used: playable keys, key-to-sounding
//! offset and tuning, per-key level evenness, brightness and attack, sustain and release, velocity and CC response,
//! keyswitches and the license of the library it comes from. This is the Rust port of `tools/instrument_card.py`;
//! the card JSON keeps that tool's schema (see [`model`]) and, in [`Schema::Reference`] mode, its exact content.
//!
//! Every note starts from CC1 = 70, CC7 = 100, CC11 = 127. A card plays:
//! 1. a key scan (every key in the range, velocity 90, 0.25 s), on its own sfizz instance;
//! 2. per key: pitch (YIN), tuning and level (velocity 90, 0.7 s);
//! 3. timbre at velocity 70 on representative keys, a 6 s held note and a release, velocities 30/70/110, and
//!    CC1/CC11 at 0 and 127, on the test note (the middle of the main range, or a kit's loudest key);
//! 4. ([`Schema::Extended`]) the test note repeated six times (round robin), a velocity curve and CC curves.
//!
//! Notes are rendered in parallel (see [`play`]) with exactly the results of the reference's serial order, except
//! for instruments with random opcodes: sfizz's random generator is process-global with a fixed seed, so those
//! reproduce the reference only when carded alone in a fresh process ([`catalog::Isolation::Subprocess`]).

pub mod catalog;
pub mod measure;
pub mod midnam;
pub mod model;
pub mod play;
pub mod sections;
pub mod sfizz;
pub mod sfztext;

pub use catalog::{Isolation, Job, RunOptions, card_many, gather_used, read_card, write_card, write_index};
pub use model::Card;

use crate::json::{OrderedMap, py_float};
use anyhow::{Result, bail};
use model::Conditions;
use play::{BASE_CC, Lane, Play, Rig};
use sections::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Which card layout to write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Schema {
    /// Exactly the fields of `tools/instrument_card.py` (for comparing against the Python catalog).
    Reference,
    /// The reference fields plus the additive measurements: `generator`, per-key `drift_cents`, `pitch.slides`,
    /// `level.holes`, `round_robin`, `velocity_curve`, `cc_curves`, and the flags "level hole", "pitch slide" and
    /// "cc dropout".
    #[default]
    Extended,
}

/// How to measure one instrument.
#[derive(Clone, Debug)]
pub struct CardOptions {
    /// Project root: card paths are relative to it; `instruments/` and `LICENSES.md` live there.
    pub root: PathBuf,
    /// Scanned key range, as typed (`36-96`, `C2-C7`); `None` = 0-127.
    pub keys: Option<String>,
    /// Keyswitch key pressed before every note.
    pub ks: Option<i64>,
    pub schema: Schema,
    /// sfizz instances rendering this card's notes at once (0 = one per available thread, at most 8).
    pub key_workers: usize,
    /// The `measured` date to write (default: today, UTC).
    pub measured: Option<String>,
}

impl Default for CardOptions {
    fn default() -> Self {
        CardOptions {
            root: flux_engine::paths::root().to_path_buf(),
            keys: None,
            ks: None,
            schema: Schema::Extended,
            key_workers: 0,
            measured: None,
        }
    }
}

impl CardOptions {
    /// The scan range (validated: 0 <= lo <= hi <= 127).
    pub fn key_range(&self) -> Result<(i64, i64)> {
        let (lo, hi) = match &self.keys {
            None => (0, 127),
            Some(s) => match sfztext::parse_key_range(s) {
                Some(r) => r,
                None => bail!("key range {s:?} is not LO-HI (e.g. 36-96 or C2-C7)"),
            },
        };
        if !(0..=127).contains(&lo) || !(0..=127).contains(&hi) || lo > hi {
            bail!("key range {lo}-{hi} must lie within 0-127, low to high");
        }
        if let Some(k) = self.ks
            && !(0..=127).contains(&k)
        {
            bail!("keyswitch {k} is not a MIDI key (0-127)");
        }
        Ok((lo, hi))
    }

    /// The card name's suffix for non-default scans (`" ks24 36-96"`), as the reference names them.
    pub fn slug_suffix(&self) -> String {
        let ks = self.ks.filter(|k| *k != 0);
        if ks.is_none() && self.keys.is_none() {
            return String::new();
        }
        format!(" ks{} {}", ks.map(|k| k.to_string()).unwrap_or_default(), self.keys.clone().unwrap_or_default())
    }

    fn workers(&self) -> usize {
        if self.key_workers > 0 {
            return self.key_workers;
        }
        rayon::current_num_threads().clamp(1, 8)
    }
}

/// Today's date (UTC) as `YYYY-MM-DD`.
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The default card name of an SFZ (`far_roads/flute`, `VCSL/Folk Harp`, ...).
pub fn default_name(root: &Path, sfz: &Path) -> String {
    sfztext::ident(root, &absolute(sfz))
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Measure one SFZ in this process. `name` defaults to [`default_name`], the slug follows from it (plus the
/// keyswitch/range suffix). Measurement failures don't fail the call: they come back as a card with `error`
/// (or with entries in `errors` for single sections). Only invalid options are an `Err`.
///
/// Instruments with random opcodes are measured serially; their values depend on what this process rendered
/// before (sfizz's random generator is global), so use [`card_many`] with subprocess isolation to reproduce them.
pub fn card(sfz: &Path, name: Option<&str>, opts: &CardOptions) -> Result<Card> {
    let (lo, hi) = opts.key_range()?;
    let t0 = Instant::now();
    let path = absolute(sfz);
    let name = name.map(str::to_string).unwrap_or_else(|| default_name(&opts.root, &path));
    let slug = sfztext::slugify(&format!("{name}{}", opts.slug_suffix()));
    let mut card = measure(&path, name, slug, lo, hi, opts);
    card.flags = flags(&card, opts.schema == Schema::Extended);
    card.seconds = Some(crate::num::round_py(t0.elapsed().as_secs_f64(), 1));
    Ok(card)
}

fn max_abs(y: &[f64]) -> f64 {
    y.iter().fold(0f64, |m, v| m.max(v.abs()))
}

fn measure(path: &Path, name: String, slug: String, lo: i64, hi: i64, opts: &CardOptions) -> Card {
    let extended = opts.schema == Schema::Extended;
    let mut card = Card {
        name,
        slug,
        sfz: sfztext::portable(&opts.root, path),
        measured: opts.measured.clone().unwrap_or_else(today_utc),
        generator: extended.then(|| format!("flux-analysis {}", env!("CARGO_PKG_VERSION"))),
        ..Default::default()
    };
    let text = match sfztext::sfz_text(path) {
        Ok(t) => t,
        Err(e) => {
            card.error = Some(format!("reading {}: {e}", path.display()));
            return card;
        }
    };
    card.about = sfztext::about(&text);
    card.conditions = Some(Conditions {
        sr: measure::SR as u32,
        cc: BASE_CC.iter().map(|(c, v)| (c.to_string(), *v as i64)).collect(),
        keyswitch: opts.ks,
        keys: [lo, hi],
    });
    card.license = Some(sfztext::license_of(&opts.root, path, &text));
    let state = sfztext::statefulness(&text);
    let random = state.random;
    let rig = Rig::new(path, opts.ks.map(|k| k as u8), state, opts.workers());
    let trace = Trace::new(path);
    // the label export loads its own sfizz instance: overlap it with loading the measuring instances
    let labels = match rig.prepare(|| midnam::labels(path)) {
        Ok(l) => l,
        Err(e) => {
            card.error = Some(format!("load failed: {e:#}"));
            return card;
        }
    };
    trace.mark("load");

    // key scan on its own instance: keyswitches pressed here would change the articulation measured later
    let scan: Vec<Play> = (lo..=hi).map(|k| Play::new(k as u8, 90, 0.25)).collect();
    let peaks = match rig.run(Lane::Scan, &scan, |_, y| crate::num::db_floor(max_abs(&y))) {
        Ok(p) => p,
        Err(e) => {
            card.error = Some(format!("{e:#}"));
            return card;
        }
    };
    trace.mark("key scan");
    let top = peaks.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let sounding: Vec<i64> = (lo..=hi).zip(&peaks).filter(|(_, v)| **v > (-80f64).max(top - 50.0)).map(|(k, _)| k).collect();
    if sounding.is_empty() {
        card.error = Some("no key makes a sound".into());
        return card;
    }
    let keys = keymap(&sounding);
    let main = keys.main;
    card.keys = Some(keys);
    let main_keys: Vec<i64> = sounding.iter().copied().filter(|k| main[0] <= *k && *k <= main[1]).collect();

    card.regions = rig.regions();
    let (klabels, clabels) = match labels {
        Ok(l) => l,
        Err(e) => {
            card.errors.insert("labels", format!("{e:#}"));
            (OrderedMap::new(), OrderedMap::new())
        }
    };
    card.cc_labels = Some(clabels);
    card.keyswitches = Some(sfztext::keyswitches(&text, &klabels));
    card.key_labels = Some(klabels);

    // per key, main range first
    let order: Vec<i64> = main_keys.iter().copied().chain(sounding.iter().copied().filter(|k| !main_keys.contains(k))).collect();
    let plays: Vec<Play> = order.iter().map(|&k| Play::new(k as u8, 90, 0.7)).collect();
    let per: Vec<(i64, model::KeyDetail)> =
        match rig.run(Lane::Detail, &plays, |p, y| (p.key as i64, key_detail(&y, p.key as i64, extended))) {
            Ok(v) => v,
            Err(e) => {
                card.errors.insert("per_key", format!("{e:#}"));
                vec![]
            }
        };
    if !per.is_empty() {
        card.per_key = Some(per.iter().map(|(k, d)| (k.to_string(), d.clone())).collect());
    }
    trace.mark("per key");
    let pitch = pitch_section(&per, extended);
    let tonal = pitch.kind != "unpitched";
    card.pitch = Some(pitch);
    match level_section(&per, &main_keys, extended) {
        Some(l) => card.level = Some(l),
        None => {
            let missing = main_keys.iter().find(|k| !per.iter().any(|(x, _)| x == *k)).copied().unwrap_or(0);
            card.errors.insert("level", format!("KeyError: {missing}"));
        }
    }

    let (timbre_keys, core, mid) = if tonal {
        // main range = the middle half of the playable keys; the test note sits in its middle
        let (core, mid) = core_and_mid(&main_keys);
        (main_keys.clone(), core, mid)
    } else {
        // drum / FX map: every key is its own sound; test the loudest
        let level = |k: i64| per.iter().find(|(x, _)| *x == k).map_or(-120.0, |(_, d)| d.level_db);
        let mut mid = sounding[0];
        for &k in &sounding[1..] {
            if level(k) > level(mid) {
                mid = k;
            }
        }
        (sounding.clone(), [sounding[0], sounding[sounding.len() - 1]], mid)
    };
    let tplays = timbre_plays(&timbre_keys);
    let nt = tplays.len();
    let mut plays = tplays.clone();
    plays.extend(sustain_plays(mid));
    plays.extend(velocity_plays(mid));
    plays.extend(cc_plays(mid));
    match rig.run(Lane::Detail, &plays, |_, y| y) {
        Ok(r) => {
            let tkeys: Vec<i64> = tplays.iter().map(|p| p.key as i64).collect();
            card.timbre = Some(timbre_section(&tkeys, &r[..nt], core));
            card.sustain = Some(sustain_section(mid, &r[nt], &r[nt + 1]));
            card.velocity = Some(velocity_section(mid, &r[nt + 2..nt + 5]));
            card.cc = Some(cc_section(&r[nt + 5..nt + 9]));
        }
        Err(e) => {
            for s in ["timbre", "sustain", "velocity", "cc"] {
                card.errors.insert(s, format!("{e:#}"));
            }
        }
    }

    trace.mark("timbre, sustain, velocity, cc");
    if extended {
        let rr = round_robin_plays(mid);
        let vc = velocity_curve_plays(mid);
        let cc = cc_curve_plays(mid);
        let mut plays = rr.clone();
        plays.extend(vc.clone());
        plays.extend(cc);
        match rig.run(Lane::Detail, &plays, |_, y| y) {
            Ok(r) => {
                let (a, b) = (rr.len(), rr.len() + vc.len());
                card.round_robin = Some(round_robin_section(mid, &r[..a], random));
                card.velocity_curve = Some(velocity_curve_section(mid, &r[a..b]));
                card.cc_curves = Some(cc_curves_section(mid, &r[b..]));
            }
            Err(e) => {
                card.errors.insert("extended", format!("{e:#}"));
            }
        }
        trace.mark("round robin, curves");
    }
    trace.done(&rig);
    card
}

/// Stage timings on stderr when `FLUX_CARD_TRACE` is set.
struct Trace {
    on: bool,
    name: String,
    t: std::cell::Cell<Instant>,
    t0: Instant,
}

impl Trace {
    fn new(path: &Path) -> Trace {
        let now = Instant::now();
        Trace {
            on: std::env::var_os("FLUX_CARD_TRACE").is_some(),
            name: path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()),
            t: std::cell::Cell::new(now),
            t0: now,
        }
    }

    fn mark(&self, stage: &str) {
        if self.on {
            eprintln!("[card {}] {stage}: {:.2} s", self.name, self.t.get().elapsed().as_secs_f64());
            self.t.set(Instant::now());
        }
    }

    fn done(&self, rig: &Rig) {
        if self.on {
            let s = rig.stats();
            let mb = rig.bytes_per_instance().map_or("?".into(), |b| format!("{:.0}", b as f64 / 1048576.0));
            eprintln!(
                "[card {}] total {:.2} s; {} sfizz instances ({mb} MB each), {} notes rendered, {} replayed",
                self.name,
                self.t0.elapsed().as_secs_f64(),
                s.instances,
                s.rendered,
                s.replayed
            );
        }
    }
}

/// `f"{x:+}"` of a Python float.
fn signed(x: f64) -> String {
    if x.is_sign_negative() { format!("-{}", py_float(-x)) } else { format!("+{}", py_float(x)) }
}

/// The card's flags (thresholds in [`sections`]); `extended` adds "level hole", "pitch slide" and "cc dropout".
pub fn flags(card: &Card, extended: bool) -> OrderedMap<String> {
    let mut f = OrderedMap::new();
    if let Some(e) = &card.error {
        f.insert("error", e.clone());
        return f;
    }
    let t = card.timbre.as_ref().and_then(|t| t.main.clone()).unwrap_or_default();
    let s = card.sustain.clone();
    let skind = s.as_ref().map_or("", |s| s.kind.as_str());
    if let Some(h) = t.hi3k_db.filter(|h| *h > HARSH) {
        f.insert("harsh", format!("hi3k {} dB in the main range (> {})", py_float(h), py_float(HARSH)));
    }
    if let Some(a) = t.attack_db.filter(|a| *a > SHARP) {
        f.insert("sharp attack", format!("attack {} dB (> {})", py_float(a), py_float(SHARP)));
    }
    // bowed and blown notes swell in by nature; a plucked or struck one that starts late has silence in its samples
    if skind == "decaying"
        && let Some(o) = t.onset_ms.filter(|o| *o > LATE)
    {
        f.insert("late start", format!("the attack arrives {} ms after note-on (> {LATE:.0} ms)", py_float(o)));
    }
    let pitch = card.pitch.clone().unwrap_or_default();
    let tonal = pitch.kind == "pitched" || pitch.kind == "mixed";
    let lv = card.level.clone().unwrap_or_default();
    if tonal
        && let (Some(j), Some([a, b])) = (lv.max_jump_db.filter(|j| *j > UNEVEN), lv.max_jump_keys)
    {
        f.insert(
            "uneven",
            format!(
                "{} dB jump between keys {a} and {b} ({} jumps > {} dB); even it out with tools/sfz_even.py",
                py_float(j),
                lv.jumps_over_6db.unwrap_or(0),
                py_float(UNEVEN)
            ),
        );
    }
    if extended
        && tonal
        && let Some(h) = lv.holes.as_ref().filter(|h| !h.is_empty())
    {
        let list: Vec<String> = h.iter().map(|(k, d)| format!("{k} (-{} dB)", py_float(*d))).collect();
        f.insert("level hole", format!("{} keys at least {HOLE_DB:.0} dB under their neighbours: {}", h.len(), list.join(", ")));
    }
    if let Some(o) = pitch.out_of_tune.as_ref().filter(|o| !o.is_empty()) {
        f.insert(
            "out of tune",
            format!(
                "{} keys > {} cents, worst {} at key {}",
                o.len(),
                py_float(DETUNED),
                signed(pitch.worst_cents.unwrap_or(0.0)),
                pitch.worst_key.unwrap_or(0)
            ),
        );
    }
    if extended
        && let Some(sl) = pitch.slides.as_ref().filter(|s| !s.is_empty())
    {
        let (mut wk, mut wc) = ("", 0.0f64);
        for (k, c) in sl.iter() {
            if c.abs() > wc.abs() {
                (wk, wc) = (k, *c);
            }
        }
        f.insert(
            "pitch slide",
            format!("{} keys move more than {SLIDE_CENTS:.0} cents during the note, most {} at key {wk}", sl.len(), signed(wc)),
        );
    }
    if let Some(u) = s.as_ref().and_then(|s| s.usable_max_note_s).filter(|u| skind.starts_with("sustained") && *u != 0.0 && *u < SHORT) {
        f.insert("short sustain", format!("the recording of a held note runs out after {} s", py_float(u)));
    }
    if extended && let Some(curves) = &card.cc_curves {
        let mut msgs = vec![];
        for (cc, c) in curves.iter().filter(|(_, c)| c.dropout) {
            let first_audible = c.rms_db.iter().position(|l| *l > -90.0).unwrap_or(0);
            let from = c.values.iter().zip(&c.rms_db).skip(first_audible).find(|(v, _)| c.silent_at.contains(v)).map_or(0, |(v, _)| *v);
            msgs.push(format!("CC{cc} silences the note at value {from} and up"));
        }
        if !msgs.is_empty() {
            f.insert("cc dropout", msgs.join("; "));
        }
    }
    if !card.errors.is_empty() {
        f.insert("partial", format!("some measurements failed: {}", card.errors.keys().collect::<Vec<_>>().join(", ")));
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_validate_ranges() {
        let mut o = CardOptions { root: PathBuf::from("."), ..Default::default() };
        assert_eq!(o.key_range().unwrap(), (0, 127));
        o.keys = Some("C2-C7".into());
        assert_eq!(o.key_range().unwrap(), (36, 96));
        o.keys = Some("96-36".into());
        assert!(o.key_range().is_err());
        o.keys = Some("nonsense".into());
        assert!(o.key_range().is_err());
        o.keys = Some("36-96".into());
        o.ks = Some(24);
        assert_eq!(o.slug_suffix(), " ks24 36-96");
        o.keys = None;
        o.ks = Some(0);
        assert_eq!(o.slug_suffix(), "");
    }

    #[test]
    fn utc_date_is_well_formed() {
        let d = today_utc();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }

    #[test]
    fn flag_messages_match_the_reference_wording() {
        let mut c = Card {
            timbre: Some(model::TimbreSection {
                main: Some(model::TimbreMedian { hi3k_db: Some(-34.9), centroid_hz: Some(1000.0), attack_db: Some(17.9), onset_ms: Some(30.0) }),
                ..Default::default()
            }),
            sustain: Some(model::Sustain { kind: "decaying".into(), ..Default::default() }),
            pitch: Some(model::Pitch {
                kind: "pitched".into(),
                worst_cents: Some(-30.7),
                worst_key: Some(100),
                out_of_tune: Some([("100".to_string(), -30.7)].into_iter().collect()),
                ..Default::default()
            }),
            level: Some(model::Level { max_jump_db: Some(23.4), max_jump_keys: Some([94, 95]), jumps_over_6db: Some(5), ..Default::default() }),
            ..Card::default()
        };
        let f = flags(&c, false);
        assert_eq!(f.get("harsh").unwrap(), "hi3k -34.9 dB in the main range (> -35.0)");
        assert_eq!(f.get("sharp attack").unwrap(), "attack 17.9 dB (> 12.0)");
        assert_eq!(f.get("late start").unwrap(), "the attack arrives 30.0 ms after note-on (> 25 ms)");
        assert_eq!(f.get("uneven").unwrap(), "23.4 dB jump between keys 94 and 95 (5 jumps > 6.0 dB); even it out with tools/sfz_even.py");
        assert_eq!(f.get("out of tune").unwrap(), "1 keys > 15.0 cents, worst -30.7 at key 100");
        c.error = Some("load failed".into());
        assert_eq!(flags(&c, false).keys().collect::<Vec<_>>(), vec!["error"]);
    }
}
