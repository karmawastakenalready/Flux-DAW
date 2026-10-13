//! MCP tools that measure with `flux-analysis`: mix problems and the tracks causing them, the master's report
//! and the moves that fix it, a reference comparison, the game's music judged as a set, spectrograms and instrument
//! report cards. `analyze` (tools.rs) reads the render as one signal; mix_check, reference_match and spectrogram also
//! take the tracks apart, from a render that keeps every track's stem and the dry master.
//!
//! mix_check and master also carry out what they suggest: each fix or move becomes a change to a copy of the
//! track's mixer or the master, the copies become set_mixer / set_master commands applied as one undo step, and the
//! song is rendered and checked again so the reply shows what it did.

use crate::state::{LastRender, Shared};
use crate::tools::{ensure_render, ensure_render_with, get, master_chain_json, rel, req, split_edges, text, tool, Content};
use anyhow::{anyhow, bail, ensure, Context, Result};
use flux_analysis::card::catalog::{self, Isolation, RunOptions};
use flux_analysis::card::CardOptions;
use flux_analysis::loudness::{check_game_with, GameInput, GameOptions};
use flux_analysis::master::{check_master, MasterIssue, MasterMove, MasterOptions, MasterReport, Part, Saturation, TonalStyle, DEESS_THRESHOLD_DB, STEREO_CROSSOVERS_HZ};
use flux_analysis::matcheq::{match_eq_spectra, EqShape, LongTermSpectrum, MatchOptions};
use flux_analysis::meta::{LayersManifest, Section};
use flux_analysis::mix::{check_mix, MixChange, MixFix, MixOptions, MixReport, RegionStems};
use flux_analysis::reference::{compare, ReferenceOptions};
use flux_analysis::spectro::SpectroOptions;
use flux_analysis::{Audio, AudioRef};
use flux_core::commands::{MasterPatch, MixerPatch, SendSpec};
use flux_core::pitch::PitchSpec;
use flux_core::{
    Channel, Command, Comp, DeEss, Detector, DynBand, EqBand, EqKind, Exciter, Imager, Limiter, Master, MbBand, Mixer, Multiband, Project,
    SatMode, TimeMap, TrackRef,
};
use flux_engine::{paths, RenderOptions, RenderOutput};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// schemas

fn sections_s(desc: &str) -> Value {
    json!({"type": "array", "description": desc, "items": {"type": "object", "properties": {
        "name": {"type": "string"}, "start_s": {"type": "number", "minimum": 0}, "end_s": {"type": "number"}},
        "required": ["name", "start_s", "end_s"]}})
}

fn detail_s(summary: &str) -> Value {
    json!({"enum": ["summary", "full"], "description": format!("summary (default) {summary}; full returns everything")})
}

fn file_s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

pub fn list() -> Vec<Value> {
    vec![
        tool("mix_check", "Find mix problems you can't hear, and the tracks causing them: tracks masking each other (which \
band, which section), buried tracks, muddy (200-500 Hz) or harsh (2-5 kHz) sections, reverb and delay smearing the attacks, \
and losses in mono. Checks the open project (rendered once more with every track kept separately; reused while the project \
is unchanged, up to 10 minutes) or a WAV/FLAC `file`. With stems, `regions` names the tracks with the most energy in \
each of master's tonal regions (sub, boom, warmth, mud, boxy, honk, harsh, sibilance, air): where to look when master \
finds a region too far off for the master EQ. In the project, `fixes` lists mixer changes with an index each; apply=true (or a list of indices) makes them one undo step (EQ bands merge with a band of the same kind within a third of \
an octave, a high-pass only rises, sends and mixes scale, the de-esser deepens), renders, checks again and replies with \
what was applied, what was skipped and why, and the counts and section measurements before and after.", json!({
            "file": file_s("a WAV/FLAC mix to check instead of the project"),
            "stems": file_s("with file: a .layers.json manifest or a folder of WAV/FLAC stems that sum to it (names the culprits)"),
            "dry": file_s("with file: the same mix without reverb and delay (enables the smear check)"),
            "sections": sections_s("judge these time ranges separately (default: the project's sections, or the manifest's)"),
            "max_conflicts": {"type": "integer", "minimum": 1, "description": "most masking collisions listed (default 12)"},
            "apply": {"anyOf": [{"type": "boolean"}, {"type": "array", "items": {"type": "integer", "minimum": 0}}],
                "description": "project only: true applies every fix, a list applies those indices (from the last mix_check of the project as it is now)"}}), &[]),
        tool("master", "Master the song for the game and check it as a mastering engineer would, without ears: loudness \
against the target (the project's: -15 LUFS integrated, -1 dBTP true peak by default), tonal balance against a style \
curve or a reference track, stereo and a mono low end, dynamics (PLR, loudness range, squashed attacks), the layer \
stems, the loop seam and technical faults (DC offset, subsonic rumble, a top end cut off by a low-rate source). Scored \
0-100: 100 has nothing to fix, every lost point has an issue saying why, and most issues carry a move for the master \
(an EQ band on both channels or on the side alone, a high-pass, the de-esser, a dynamic EQ band for a range that is too \
loud only at times, the exciter for a dull or cut-off top, an imager band, mono below, width, the limiter, the target) \
with an index. Actions: check (default; renders if stale, then a few seconds; with layer stems when the project has \
layers); apply (the check's moves, all or `apply` indices, as one undo step, then renders and checks again: before and \
after; an EQ move folds into a master EQ band of the same kind and channel within a third of an octave, and a band stops \
at +-6 dB: past that the balance belongs in the mix; a high-pass only rises, the de-esser and a dynamic EQ band only \
deepen, the exciter only blends in more; each band and setting changes once a round); match (EQ the master toward `reference`); auto (check, apply, render and check again up to `passes` times, stopping when a pass \
gains under 1 point and undoing a pass that loses points); preset (set a named master chain). Every change is one undo \
step in activity. Mix first (mix_check), master last.", json!({
            "action": {"enum": ["check", "apply", "match", "auto", "preset"], "description": "default check"},
            "style": {"enum": ["balanced", "cinematic", "electronic", "acoustic"], "description": "the tonal target without a reference, \
measured from about 75 professional masters by 6 to 8 artists each, with a tolerance of 2.5 times how far they spread \
(default balanced, band-based game and pop music): cinematic has a fuller low-mid body and darker highs (orchestral, trailer), \
electronic a deep sub and the brightest top (synths, beats), acoustic little deep bass, a full 100-500 Hz body and dark highs \
(acoustic instruments, small ensembles; its masters differ most, so its tolerance is widest and a reference checks \
far closer). The check \
names the closest style from 100 Hz up (tonal.closest_style, with each style's deviation): a closer_style issue says the song \
reads as another style"},
            "reference": file_s("WAV/FLAC (e.g. refs/some_song.flac): its spectrum at equal loudness becomes the tonal target; required for match"),
            "target_lufs": {"type": "number", "minimum": -40, "maximum": 0, "description": "check against this loudness instead of the project's; apply, match, auto and preset also set it on the master"},
            "ceiling_dbtp": {"type": "number", "minimum": -20, "maximum": 0, "description": "as target_lufs, for the true-peak ceiling"},
            "apply": {"type": "array", "items": {"type": "integer", "minimum": 0}, "description": "apply: the issue indices to apply, \
from the last check of the project as it is now (default: every move). A move that would put the loudness target over 1 LU \
from the delivery target is applied only when picked here"},
            "amount": {"type": "number", "exclusiveMinimum": 0, "maximum": 1, "description": "match: the share of the tonal difference to correct (default 0.6: most of the way, keeping the song's character)"},
            "passes": {"type": "integer", "minimum": 1, "maximum": 5, "description": "auto: most rounds (default 3)"},
            "name": {"enum": PRESETS, "description": "preset: transparent (the limiter at its defaults, no multiband; glue and mono \
untouched), game_loop (gentle RMS glue 2:1 with its detector high-passed at 100 Hz, the default multiband, mono below 120 Hz: \
the usual start for game music), cinematic (slow soft glue, a gentle slow multiband, a smooth limiter, mono below 100 Hz: \
orchestral and wide dynamics), electronic (tighter glue, a firmer multiband, a fast limiter that clips more, mono below 150 \
Hz: synths and beats), punchy (parallel glue with a slow attack, no multiband, a fast limiter that clips more: drums up \
front), acoustic (light soft-knee glue, no multiband, a slow limiter: small ensembles). A preset sets the glue, multiband, \
limiter and mono_below_hz; never the EQ, the width, the target or the ceiling (give target_lufs or ceiling_dbtp for those)"},
            "detail": detail_s("gives the score, the loudness, what is outside the tonal tolerance, the stereo and dynamics verdicts, the issues and the master chain's levels")}), &[]),
        tool("reference_match", "Compare the mix with a reference track at equal loudness: tonal balance, dynamics, \
transients, width, low end and mono, as suggestions ranked by score (the difference over its just-noticeable step; 1 = just \
noticeable), for the whole song and per section. In the open project each suggestion names the tracks that carry the \
frequency range. Keep reference files in refs/ (git-ignored, never shipped).", json!({
            "reference": file_s("WAV/FLAC reference track (e.g. refs/some_song.flac)"),
            "file": file_s("a WAV/FLAC mix to compare instead of the project"),
            "stems": file_s("with file: a .layers.json manifest or a folder of stems, to name the tracks behind a suggestion"),
            "sections": sections_s("mix sections compared one by one (default: the project's sections)"),
            "reference_sections": sections_s("the reference's sections: a mix section is compared with the one of the same name, else with the whole reference"),
            "max_suggestions": {"type": "integer", "minimum": 1, "description": "per section (default 5)"},
            "detail": detail_s("leaves out the two full profiles and the per-section differences")}), &["reference"]),
        tool("game_audio_check", "Judge the game's music as a set: every track at the loudness target (-15 LUFS +-1) and \
under the true-peak ceiling (-1 dBTP) and the 20 MB file limit, loops seamless at the wrap, layer stems summing to their \
master, and no track that will jump out next to the others (loudness outliers, loud passages, over-limiting). Default: \
the deliverables in output/ (FLAC files, WAVs without a FLAC beside them and .layers.json sets; stems/ folders skipped).", json!({
            "paths": {"type": "array", "items": {"type": "string"}, "description": "files or folders to check instead of the deliverables in output/"},
            "recursive": {"type": "boolean", "description": "with paths: search folders recursively (default false)"},
            "include_project": {"type": "boolean", "description": "also judge the open project's render beside the files (rendered first if stale; default false)"},
            "target_lufs": {"type": "number"}, "ceiling_dbtp": {"type": "number"},
            "max_file_bytes": {"type": "integer", "description": "default 20000000"},
            "detect_loops": {"type": "boolean", "description": "decide which files loop from their export report, song metadata or name (default true; false: all one-shots)"},
            "detail": detail_s("keeps each track's headline numbers and flags")}), &[]),
        tool("spectrogram", "See where the energy is over time: a log-frequency spectrogram (30 Hz at the bottom of each \
band to 16 kHz at the top, 80 dB range, brighter = louder), one band per source stacked top to bottom in the order given. \
Sources: \"master\" (the render), \"dry\" (the master without the reverb return and the delays), track names (each track \
as it sounds in the master) or WAV/FLAC files.", json!({
            "sources": {"type": "array", "items": {"type": "string"}, "maxItems": 8, "description": "default [\"master\"]"},
            "from_bar": {"type": "integer", "minimum": 1}, "to_bar": {"type": "integer", "minimum": 1, "description": "last bar shown (inclusive)"},
            "start_s": {"type": "number", "minimum": 0}, "end_s": {"type": "number"},
            "width": {"type": "integer", "minimum": 100, "maximum": 4000, "description": "pixels (default 1200)"},
            "height": {"type": "integer", "minimum": 40, "maximum": 600, "description": "pixels per band (default 160)"},
            "save": file_s("also write the PNG here")}), &[]),
        tool("instrument_card", "Measure SFZ instruments the way the catalog does (catalog/<slug>.json): playable keys, \
level and tuning per key, dynamics, sustain and release, brightness and attack, velocity layers, round robins, \
keyswitches and CC response, with flags for problems (level holes, detuned keys, sharp attacks...). A saved card newer \
than its SFZ is returned without measuring again unless refresh=true. Measuring takes from seconds to a few minutes per \
instrument.", json!({
            "sfz": {"type": "array", "items": {"type": "string"}, "minItems": 1},
            "name": {"type": "string", "description": "card name (one SFZ only; default from the path)"},
            "keys": {"type": "string", "description": "scanned key range, e.g. \"36-96\" or \"C2-C7\" (default 0-127)"},
            "ks": {"anyOf": [{"type": "integer", "minimum": 0, "maximum": 127}, {"type": "string"}], "description": "keyswitch pressed before every note"},
            "refresh": {"type": "boolean", "description": "measure again even if a fresh card exists"},
            "save": {"type": "boolean", "description": "write the cards to catalog/ and update catalog/README.md (default true)"},
            "detail": detail_s("leaves out the per-key table")}), &["sfz"]),
    ]
}

pub fn call(sh: &Arc<Shared>, name: &str, a: &Value) -> Result<Vec<Content>> {
    match name {
        "mix_check" => mix_check(sh, a),
        "master" => master(sh, a),
        "reference_match" => reference_match(sh, a),
        "game_audio_check" => game_audio_check(sh, a),
        "spectrogram" => spectrogram(sh, a),
        "instrument_card" => instrument_card(a),
        _ => bail!("unknown tool '{name}'"),
    }
}

// ---------------------------------------------------------------------------------------------
// helpers

/// Every float to 2 decimals: the reports carry many measurements, and more digits only cost tokens.
fn round2(v: &mut Value) {
    match v {
        Value::Number(n) if n.is_f64() => {
            if let Some(x) = n.as_f64() {
                *v = json!((x * 100.0).round() / 100.0);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(round2),
        Value::Object(m) => m.values_mut().for_each(round2),
        _ => {}
    }
}

fn reply(mut v: Value) -> Result<Vec<Content>> {
    round2(&mut v);
    text(v)
}

fn full(a: &Value) -> Result<bool> {
    Ok(match get::<String>(a, "detail")?.as_deref() {
        None | Some("summary") => false,
        Some("full") => true,
        Some(d) => bail!("unknown detail '{d}' (summary, full)"),
    })
}

fn is_audio(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("flac"))
}

fn read_audio(p: &str) -> Result<(PathBuf, Audio)> {
    let path = paths::resolve(p);
    let audio = Audio::read(&path).with_context(|| format!("reading {}", rel(&path)))?;
    Ok((path, audio))
}

/// Named stems, decoded.
type Stems = Vec<(String, Audio)>;

/// Stems for a mix file: a `.layers.json` manifest's layer files (and its sections), or every WAV/FLAC in a folder.
fn load_stems(spec: Option<&str>) -> Result<(Stems, Vec<Section>)> {
    let Some(s) = spec else { return Ok((vec![], vec![])) };
    let p = paths::resolve(s);
    let (files, sections) = if p.is_dir() {
        let mut found: Vec<PathBuf> =
            std::fs::read_dir(&p).with_context(|| format!("listing {}", rel(&p)))?.filter_map(|e| e.ok().map(|e| e.path())).filter(|f| is_audio(f)).collect();
        found.sort();
        let named = found.into_iter().map(|f| (f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), f)).collect();
        (named, vec![])
    } else {
        let m = LayersManifest::read(&p)?;
        (m.layer_paths(&p), m.sections())
    };
    ensure!(!files.is_empty(), "no stems in {}", rel(&p));
    // decoding is the slow part: one thread per stem
    let stems = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .iter()
            .map(|(name, f)| s.spawn(move || Audio::read(f).with_context(|| format!("reading {}", rel(f))).map(|x| (name.clone(), x))))
            .collect();
        handles.into_iter().map(|h| h.join().map_err(|_| anyhow!("a stem decoder panicked"))?).collect::<Result<Vec<_>>>()
    })?;
    Ok((stems, sections))
}

fn view(sr: u32, (l, r): &(Vec<f32>, Vec<f32>)) -> AudioRef<'_> {
    let n = l.len().min(r.len());
    AudioRef { sr, left: &l[..n], right: &r[..n] }
}

/// -90 dBFS: a stem this quiet says nothing about the mix.
const SILENT: f32 = 3.2e-5;

/// The tracks that sound in a separated render, named.
fn track_stems(out: &RenderOutput) -> Vec<(&str, AudioRef<'_>)> {
    out.track_stems
        .iter()
        .filter(|(_, (l, r))| l.iter().chain(r.iter()).any(|x| x.abs() > SILENT))
        .map(|(name, buf)| (name.as_str(), view(out.sr, buf)))
        .collect()
}

/// The main render of the current revision, rendered now if stale.
fn current_render(sh: &Arc<Shared>) -> Result<Arc<LastRender>> {
    let fresh = {
        let st = sh.lock();
        st.render.as_ref().filter(|r| r.revision == st.session.revision).cloned()
    };
    if let Some(r) = fresh {
        return Ok(r);
    }
    ensure_render(sh, 900.0)?;
    sh.lock().render.clone().ok_or_else(|| anyhow!("nothing rendered"))
}

// ---------------------------------------------------------------------------------------------
// the separated render

/// The open project rendered with every track's stem and the dry master kept (`RenderOptions::track_stems` and
/// `dry`). The main render doesn't keep them (a buffer per track), so the measurements that need them render once
/// more; the result is reused while the project is unchanged.
pub struct Separated {
    key: u64,
    made: Instant,
    pub revision: u64,
    pub project: Project,
    pub out: RenderOutput,
}

const SEPARATED_TTL: Duration = Duration::from_secs(600);
/// Larger separated renders are measured and dropped rather than kept (a 3-minute song with 15 tracks is about
/// 1.2 GB).
const SEPARATED_KEEP_BYTES: usize = 2 << 30;
/// Separated renders estimated to need more memory than this are refused.
const SEPARATED_BUDGET_BYTES: usize = 4 << 30;

fn separated_cache() -> &'static Mutex<Option<Arc<Separated>>> {
    static C: OnceLock<Mutex<Option<Arc<Separated>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// A hash of everything in the project: equal keys render the same.
fn project_key(p: &Project) -> Result<u64> {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(p)?.hash(&mut h);
    Ok(h.finish())
}

pub fn separated(sh: &Arc<Shared>) -> Result<Arc<Separated>> {
    let (project, revision) = {
        let st = sh.lock();
        (st.session.project.clone(), st.session.revision)
    };
    let key = project_key(&project)?;
    if let Some(s) = separated_cache().lock().as_ref()
        && s.key == key
        && s.made.elapsed() < SEPARATED_TTL
    {
        return Ok(s.clone());
    }
    // while rendering, each track holds its processed buffer and its stem: two stereo buffers per track
    let frames = flux_engine::render::raw_frames(&project);
    let need = frames * 8 * (2 * project.tracks.len() + 4);
    ensure!(
        need <= SEPARATED_BUDGET_BYTES,
        "the project is too long to keep every track separately ({:.1} GB > {:.0} GB): export it and check the file with its \
layer stems instead",
        need as f64 / 1e9,
        SEPARATED_BUDGET_BYTES as f64 / 1e9
    );
    // a main render in progress plays the same plugin instances: let it finish first
    sh.wait_render(Duration::from_secs(900));
    let out = flux_engine::render(&project, &RenderOptions { track_stems: true, dry: true, ..Default::default() })?;
    let bytes = out.left.len() * 8 * (2 + out.track_stems.len() + out.layers.len());
    let s = Arc::new(Separated { key, made: Instant::now(), revision, project, out });
    let keep = bytes <= SEPARATED_KEEP_BYTES;
    *separated_cache().lock() = keep.then(|| s.clone());
    if keep {
        // free the memory when it expires, not at the next measurement
        let weak = Arc::downgrade(&s);
        std::thread::Builder::new().name("separated-expiry".into()).spawn(move || {
            std::thread::sleep(SEPARATED_TTL);
            let mut c = separated_cache().lock();
            if c.as_ref().is_some_and(|k| weak.upgrade().is_some_and(|w| Arc::ptr_eq(k, &w))) {
                *c = None;
            }
        })?;
    }
    Ok(s)
}

// ---------------------------------------------------------------------------------------------
// mix_check / reference_match

/// Which suggestions to carry out: all of them, or these indices.
enum Pick {
    All,
    These(Vec<usize>),
}

impl Pick {
    /// `apply` as mix_check takes it: true, false or a list of indices.
    fn of(a: &Value) -> Result<Option<Pick>> {
        Ok(match a.get("apply") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(Pick::All),
            Some(v) => Some(Pick::These(serde_json::from_value(v.clone()).context("argument 'apply': give true or a list of indices")?)),
        })
    }

    /// The chosen indices out of `n`, in order, each once.
    fn indices(&self, n: usize) -> Vec<usize> {
        match self {
            Pick::All => (0..n).collect(),
            Pick::These(v) => {
                let mut seen = BTreeSet::new();
                v.iter().copied().filter(|i| seen.insert(*i)).collect()
            }
        }
    }
}

/// The last project mix_check that replied: the separated render it measured and its options, so indices picked from
/// it can be told apart from a check of another state.
fn last_mix_check() -> &'static Mutex<Option<(u64, String)>> {
    static C: OnceLock<Mutex<Option<(u64, String)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn mix_options_key(o: &MixOptions) -> Result<String> {
    Ok(format!("{}|{}", o.max_conflicts, serde_json::to_string(&o.sections)?))
}

fn check_separated(s: &Separated, o: &MixOptions) -> Result<MixReport> {
    let dry = s.out.dry.as_ref().map(|d| view(s.out.sr, d));
    check_mix(AudioRef::master(&s.out), &track_stems(&s.out), dry, o)
}

/// The fixes with their indices.
fn fixes_json(fixes: &[MixFix]) -> Result<Value> {
    let mut v = vec![];
    for (i, f) in fixes.iter().enumerate() {
        let mut x = serde_json::to_value(f)?;
        x["index"] = json!(i);
        v.push(x);
    }
    Ok(json!(v))
}

/// The tonal regions someone plays in, with the tracks playing there.
fn regions_json(regions: &[RegionStems]) -> Result<Value> {
    Ok(serde_json::to_value(regions.iter().filter(|r| !r.stems.is_empty()).collect::<Vec<_>>())?)
}

/// A mix report in numbers, to compare before and after: how many findings, and each section's mud and harshness
/// against pink noise, mono fold-down loss and reverb smear.
fn mix_metrics(r: &MixReport) -> Value {
    let flags: usize =
        r.tone.iter().map(|t| t.flags.len()).sum::<usize>() + r.mono.iter().map(|m| m.flags.len()).sum::<usize>() + r.transients.iter().map(|t| t.flags.len()).sum::<usize>();
    let sections: Vec<Value> = r
        .tone
        .iter()
        .map(|t| {
            let mono = r.mono.iter().find(|m| m.section == t.section);
            let tr = r.transients.iter().find(|x| x.section == t.section);
            json!({"section": t.section, "mud_vs_pink_db": t.mud_vs_pink_db, "harsh_vs_pink_db": t.harsh_vs_pink_db,
                "fold_down_lu": mono.and_then(|m| m.fold_down_lu), "smear_db": tr.and_then(|x| x.smear_db)})
        })
        .collect();
    json!({"conflicts": r.conflicts.len(), "buried": r.buried.len(), "flags": flags, "fixes": r.fixes.len(), "sections": sections})
}

fn mix_check(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let sections = get::<Vec<Section>>(a, "sections")?;
    let max_conflicts = get::<usize>(a, "max_conflicts")?.unwrap_or(12).max(1);
    let pick = Pick::of(a)?;
    if let Some(f) = get::<String>(a, "file")? {
        ensure!(pick.is_none(), "apply changes the open project's mixers: run mix_check without `file` to apply its fixes");
        let (path, mix) = read_audio(&f)?;
        let (stems, manifest_sections) = load_stems(get::<String>(a, "stems")?.as_deref())?;
        let dry = get::<String>(a, "dry")?.map(|d| read_audio(&d)).transpose()?;
        let views: Vec<(&str, AudioRef)> = stems.iter().map(|(n, x)| (n.as_str(), x.view())).collect();
        let o = MixOptions { sections: sections.unwrap_or(manifest_sections), max_conflicts, ..Default::default() };
        let report = check_mix(mix.view(), &views, dry.as_ref().map(|d| d.1.view()), &o)?;
        let mut v = serde_json::to_value(&report)?;
        v["regions"] = regions_json(&report.regions)?;
        v["source"] = json!(rel(&path));
        return reply(v);
    }
    let s = separated(sh)?;
    let o = MixOptions { sections: sections.unwrap_or_else(|| Section::of_render(&s.out)), max_conflicts, ..Default::default() };
    let okey = mix_options_key(&o)?;
    if let Some(Pick::These(_)) = &pick {
        let seen = last_mix_check().lock().as_ref().is_some_and(|(k, opts)| *k == s.key && *opts == okey);
        ensure!(
            seen,
            "the project changed since the last mix_check (or it used other sections): run mix_check again and pick the indices from its \
fixes, or give apply=true"
        );
    }
    let report = check_separated(&s, &o)?;
    let Some(pick) = pick else {
        *last_mix_check().lock() = Some((s.key, okey));
        let mut v = serde_json::to_value(&report)?;
        v["fixes"] = fixes_json(&report.fixes)?;
        v["regions"] = regions_json(&report.regions)?;
        v["source"] = json!(format!("project revision {}", s.revision));
        if !report.fixes.is_empty() {
            v["next"] = json!("mix_check apply=true applies every fix (or apply=[indices] some) as one undo step and checks again");
        }
        return reply(v);
    };
    let chosen = pick.indices(report.fixes.len());
    let (applied, skipped, revision) = sh.with_session(|ses| -> Result<_> {
        ensure!(project_key(&ses.project)? == s.key, "the project changed while it was checked: run mix_check again");
        let (cmds, applied, skipped) = plan_mix(&ses.project, &report.fixes, &chosen);
        if cmds.is_empty() {
            return Ok((applied, skipped, None));
        }
        let mut names: Vec<&str> = applied.iter().filter_map(|x| x["track"].as_str()).collect();
        names.dedup();
        let label = format!("mix_check: {} fix{} on {}", applied.len(), if applied.len() == 1 { "" } else { "es" }, names.join(", "));
        ses.apply_batch_labeled(&cmds, flux_core::Source::Agent, Some(&label))?;
        Ok((applied, skipped, Some(ses.revision)))
    })?;
    let Some(revision) = revision else {
        *last_mix_check().lock() = Some((s.key, okey));
        return reply(json!({"applied": [], "skipped": skipped, "fixes": fixes_json(&report.fixes)?,
            "note": "nothing was changed: every chosen fix was skipped"}));
    };
    let s2 = separated(sh)?;
    let after = check_separated(&s2, &o)?;
    *last_mix_check().lock() = Some((s2.key, okey));
    reply(json!({
        "applied": applied, "skipped": skipped, "revision": revision,
        "before": mix_metrics(&report), "after": mix_metrics(&after), "fixes": fixes_json(&after.fixes)?,
        "regions": regions_json(&after.regions)?,
        "next": "one undo reverts the whole batch. `fixes` are the new check's, with new indices: apply them the same way, or stop when \
what is left is a matter of taste",
    }))
}

fn reference_match(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (ref_path, reference) = read_audio(&req::<String>(a, "reference")?)?;
    let sections = get::<Vec<Section>>(a, "sections")?;
    let opts = |mix_sections: Vec<Section>| -> Result<ReferenceOptions> {
        Ok(ReferenceOptions {
            mix_sections,
            reference_sections: get::<Vec<Section>>(a, "reference_sections")?.unwrap_or_default(),
            section_suggestions: get::<usize>(a, "max_suggestions")?.unwrap_or(5).max(1),
        })
    };
    let (source, report) = match get::<String>(a, "file")? {
        Some(f) => {
            let (path, mix) = read_audio(&f)?;
            let (stems, manifest_sections) = load_stems(get::<String>(a, "stems")?.as_deref())?;
            let views: Vec<(&str, AudioRef)> = stems.iter().map(|(n, x)| (n.as_str(), x.view())).collect();
            (rel(&path), compare(mix.view(), reference.view(), &views, &opts(sections.unwrap_or(manifest_sections))?)?)
        }
        None => {
            let s = separated(sh)?;
            let o = opts(sections.unwrap_or_else(|| Section::of_render(&s.out)))?;
            (format!("project revision {}", s.revision), compare(AudioRef::master(&s.out), reference.view(), &track_stems(&s.out), &o)?)
        }
    };
    let mut v = serde_json::to_value(&report)?;
    if !full(a)? {
        if let Some(w) = v["whole"].as_object_mut() {
            w.remove("mix");
            w.remove("reference");
        }
        for s in v["sections"].as_array_mut().into_iter().flatten() {
            if let Some(o) = s.as_object_mut() {
                o.remove("diff");
            }
        }
    }
    v["source"] = json!(source);
    v["reference"] = json!(rel(&ref_path));
    reply(v)
}

// ---------------------------------------------------------------------------------------------
// carrying out suggestions: mix_check's fixes on track mixers, master's moves on the master

/// What a suggestion did, or why it was left out.
type Outcome = std::result::Result<String, String>;

/// Largest gain a track's EQ band built up by mix_check's fixes may reach, dB.
const EQ_MAX_DB: f64 = 12.0;
/// Largest gain a master EQ band built up by master moves may reach, dB: past it the balance belongs in the mix.
const MASTER_EQ_MAX_DB: f64 = 6.0;
/// Two EQ bands of the same kind closer than this (octaves) are one band to the suggestions.
const EQ_MERGE_OCT: f64 = 1.0 / 3.0;

fn hz_text(f: f64) -> String {
    if f >= 1000.0 { format!("{} kHz", (f / 100.0).round() / 10.0) } else { format!("{f:.0} Hz") }
}

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn kind_name(k: EqKind) -> &'static str {
    match k {
        EqKind::Peak => "peak",
        EqKind::LowShelf => "low shelf",
        EqKind::HighShelf => "high shelf",
        EqKind::Notch => "notch",
        EqKind::Tilt => "tilt",
        EqKind::HighPass => "high-pass",
        EqKind::LowPass => "low-pass",
    }
}

/// An EQ band's kind in words, with the part of the stereo signal it works on unless that's both channels.
fn band_name(b: &EqBand) -> String {
    match b.channel {
        Channel::Stereo => kind_name(b.kind).to_string(),
        Channel::Mid => format!("mid {}", kind_name(b.kind)),
        Channel::Side => format!("side {}", kind_name(b.kind)),
    }
}

fn eq_kind(s: EqShape) -> EqKind {
    match s {
        EqShape::Peak => EqKind::Peak,
        EqShape::LowShelf => EqKind::LowShelf,
        EqShape::HighShelf => EqKind::HighShelf,
    }
}

fn channel(p: Part) -> Channel {
    match p {
        Part::Stereo => Channel::Stereo,
        Part::Mid => Channel::Mid,
        Part::Side => Channel::Side,
    }
}

fn sat_mode(s: Saturation) -> SatMode {
    match s {
        Saturation::Tape => SatMode::Tape,
        Saturation::Tube => SatMode::Tube,
    }
}

/// The band of an EQ that `band` would fold into: the same kind on the same channel, within a third of an octave.
fn near_band(eq: &[EqBand], band: &EqBand) -> Option<usize> {
    eq.iter().position(|b| {
        b.kind == band.kind && b.channel == band.channel && b.hz > 0.0 && band.hz > 0.0 && (b.hz / band.hz).log2().abs() <= EQ_MERGE_OCT + 1e-9
    })
}

/// Add `band` to an EQ, or fold it into the band `near_band` finds: the gains add (that band keeps its frequency and
/// Q), clamped to +-`max_db`, and a band that ends up within 0.1 dB of flat is removed. A band already at the limit
/// in the change's direction is left as it is (Err).
fn merge_eq(eq: &mut Vec<EqBand>, band: EqBand, max_db: f64) -> Outcome {
    match near_band(eq, &band) {
        Some(i) => {
            let old = eq[i];
            let db = r2((old.db + band.db).clamp(-max_db, max_db));
            if (db - old.db).abs() < 0.05 {
                return Err(format!("the {} at {} is already at {:+.1} dB, as far as these changes go", band_name(&old), hz_text(old.hz), old.db));
            }
            if db.abs() < 0.1 {
                eq.remove(i);
                Ok(format!("removed the {} at {} (the change cancelled it)", band_name(&old), hz_text(old.hz)))
            } else {
                eq[i].db = db;
                Ok(format!("{} at {}: {:+.1} -> {db:+.1} dB", band_name(&old), hz_text(old.hz), old.db))
            }
        }
        None => Ok(add_band(eq, band, max_db)),
    }
}

/// Add `band` to an EQ as a band of its own, within the EQ's ranges and +-`max_db`.
fn add_band(eq: &mut Vec<EqBand>, band: EqBand, max_db: f64) -> String {
    let b = EqBand { hz: band.hz.clamp(20.0, 20000.0), db: r2(band.db.clamp(-max_db, max_db)), q: band.q.clamp(0.1, 10.0), ..band };
    eq.push(b);
    format!("added a {} of {:+.1} dB at {}, Q {:.2}", band_name(&b), b.db, hz_text(b.hz), b.q)
}

/// The track a fix names: its exact name, else the one track with that name in another case. A missing or ambiguous
/// name is left out rather than guessed.
fn resolve_track(p: &Project, name: &str) -> std::result::Result<usize, String> {
    let exact: Vec<usize> = p.tracks.iter().enumerate().filter(|(_, t)| t.name == name).map(|(i, _)| i).collect();
    let found = if exact.is_empty() {
        p.tracks.iter().enumerate().filter(|(_, t)| t.name.eq_ignore_ascii_case(name)).map(|(i, _)| i).collect()
    } else {
        exact
    };
    match found.as_slice() {
        [i] => Ok(*i),
        [] => Err(format!("no track is named '{name}' now (renamed or removed since the check?)")),
        many => Err(format!("{} tracks are named '{name}': rename one so the fix can tell them apart, then check again", many.len())),
    }
}

/// Whether a track is an aux return whose sound is a reverb: a built-in reverb, or (a plugin reverb) inserts and
/// neither reverb nor delay of its own.
fn reverb_return(m: &Mixer) -> bool {
    m.reverb.is_some() || (m.delay.is_none() && !m.inserts.is_empty())
}

/// Scale a level (a send, a mix: 0..1) by `scale`, rounded to 3 decimals: what changed, in words, if anything did.
fn scale_level(what: &str, x: &mut f64, scale: f64) -> Option<String> {
    let old = *x;
    let new = ((old * scale).clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
    if new == old {
        return None;
    }
    *x = new;
    Some(format!("{what} {old} -> {new}"))
}

/// One mix_check fix carried out on a copy of track `ti`'s mixer (`p` is the project, for the tracks it sends to).
fn mix_change(p: &Project, ti: usize, m: &mut Mixer, c: &MixChange) -> Outcome {
    let me = p.tracks[ti].id;
    match *c {
        MixChange::EqCut { hz, db, q } | MixChange::EqBoost { hz, db, q } => merge_eq(&mut m.eq, EqBand::new(EqKind::Peak, hz, db, q), EQ_MAX_DB)
            .map(|d| format!("EQ: {d}"))
            .map_err(|e| format!("{e}: change the part itself (its sound, octave or level) instead")),
        MixChange::HighPass { hz } => {
            if m.hpf_hz >= hz - 0.5 {
                return Err(format!("its high-pass is already at {} (the fix asks for {})", hz_text(m.hpf_hz), hz_text(hz)));
            }
            let old = m.hpf_hz;
            m.hpf_hz = hz.round();
            Ok(if old > 0.0 { format!("high-pass {} -> {}", hz_text(old), hz_text(m.hpf_hz)) } else { format!("high-pass at {}", hz_text(m.hpf_hz)) })
        }
        MixChange::Gain { db } => {
            let old = m.gain_db;
            let new = r2((old + db).clamp(-24.0, 24.0));
            if (new - old).abs() < 0.05 {
                return Err(format!("its fader is already at {old:+.1} dB, the limit: change the instrument's own level instead"));
            }
            m.gain_db = new;
            Ok(format!("fader {old:+.1} -> {new:+.1} dB"))
        }
        MixChange::ReverbSend { scale } => {
            let mut did = vec![];
            did.extend(scale_level("reverb send", &mut m.reverb_send, scale));
            for s in &mut m.sends {
                if let Some(to) = p.tracks.iter().find(|t| t.id == s.to)
                    && reverb_return(&to.mixer)
                {
                    did.extend(scale_level(&format!("send to '{}'", to.name), &mut s.level, scale));
                }
            }
            let fed = p.tracks.iter().any(|t| t.mixer.output == Some(me) || t.mixer.sends.iter().any(|s| s.to == me));
            if !fed && let Some(r) = &mut m.reverb {
                did.extend(scale_level("its reverb's mix", &mut r.mix, scale));
            }
            if did.is_empty() {
                return Err("it has no reverb send, no send to a reverb return and no reverb of its own left to turn down: its reverb \
comes from an insert or its instrument (turn that down by hand)"
                    .into());
            }
            Ok(did.join(", "))
        }
        MixChange::DelayMix { scale } => {
            let mut did = vec![];
            if let Some(d) = &mut m.delay {
                did.extend(scale_level("delay mix", &mut d.mix, scale));
            }
            for s in &mut m.sends {
                if let Some(to) = p.tracks.iter().find(|t| t.id == s.to)
                    && to.mixer.delay.is_some()
                    && to.mixer.reverb.is_none()
                {
                    did.extend(scale_level(&format!("send to '{}'", to.name), &mut s.level, scale));
                }
            }
            if did.is_empty() {
                return Err("it has no delay and no send to a delay return left to turn down: its echoes come from an insert or its \
instrument (turn that down by hand)"
                    .into());
            }
            Ok(did.join(", "))
        }
        MixChange::MonoBelow { hz } => {
            let hz = hz.clamp(0.0, 1000.0);
            if m.mono_below_hz >= hz - 0.5 {
                return Err(format!("it is already mono below {}", hz_text(m.mono_below_hz)));
            }
            let old = m.mono_below_hz;
            m.mono_below_hz = hz.round();
            Ok(if old > 0.0 { format!("mono below {} -> {}", hz_text(old), hz_text(m.mono_below_hz)) } else { format!("mono below {}", hz_text(m.mono_below_hz)) })
        }
        MixChange::DeEss { hz, range_db } => {
            deess_change(&mut m.deess, hz, range_db, DeEss::default().threshold_db).map_err(|e| format!("its {e}: cut the band with its EQ instead"))
        }
    }
}

/// Turn on a de-esser above `hz` with `range_db`, or deepen the one there: its band reaches down to `hz` and its
/// range to `range_db`, and when both already do, its threshold comes down 3 dB (to -30) so it catches more.
fn deess_change(slot: &mut Option<DeEss>, hz: f64, range_db: f64, threshold_db: f64) -> Outcome {
    let (hz, range_db) = (hz.clamp(1000.0, 16000.0), range_db.clamp(-24.0, 0.0));
    match slot {
        None => {
            let d = DeEss { hz: hz.round(), range_db, threshold_db };
            *slot = Some(d);
            let at = match d.threshold_db {
                t if t.abs() < 0.5 => "at".to_string(),
                t if t < 0.0 => format!("{:.0} dB under", -t),
                t => format!("{t:.0} dB over"),
            };
            Ok(format!("added a de-esser above {}, range {:.1} dB, threshold {at} the band's loud level", hz_text(d.hz), d.range_db))
        }
        Some(d) => {
            let (h, r) = (d.hz.min(hz.round()), d.range_db.min(range_db));
            if h < d.hz || r < d.range_db {
                let was = format!("{}, {:.1} dB", hz_text(d.hz), d.range_db);
                (d.hz, d.range_db) = (h, r);
                return Ok(format!("de-esser {was} -> above {}, {:.1} dB", hz_text(h), r));
            }
            let t = (d.threshold_db - 3.0).max(-30.0);
            if t >= d.threshold_db {
                return Err(format!("de-esser is already at its deepest ({:.0} dB threshold)", d.threshold_db));
            }
            let old = d.threshold_db;
            d.threshold_db = t;
            Ok(format!("de-esser threshold {old:.0} -> {t:.0} dB, so it catches more"))
        }
    }
}

/// The patch that turns mixer `a` into `b`, for the settings the fixes change; None if nothing did.
fn mixer_patch(a: &Mixer, b: &Mixer) -> Option<MixerPatch> {
    let mut p = MixerPatch::default();
    let mut any = false;
    let mut set = |changed: bool| {
        any |= changed;
        changed
    };
    if set(a.gain_db != b.gain_db) {
        p.gain_db = Some(b.gain_db);
    }
    if set(a.hpf_hz != b.hpf_hz) {
        p.hpf_hz = Some(b.hpf_hz);
    }
    if set(a.mono_below_hz != b.mono_below_hz) {
        p.mono_below_hz = Some(b.mono_below_hz);
    }
    if set(a.reverb_send != b.reverb_send) {
        p.reverb_send = Some(b.reverb_send);
    }
    if set(a.eq != b.eq) {
        p.eq = Some(b.eq.clone());
    }
    if set(a.delay != b.delay) {
        p.delay = Some(b.delay);
    }
    if set(a.deess != b.deess) {
        p.deess = Some(b.deess);
    }
    if set(a.reverb != b.reverb) {
        p.reverb = Some(b.reverb);
    }
    if set(a.sends != b.sends) {
        p.sends = Some(b.sends.iter().map(|s| SendSpec { to: TrackRef::Id(s.to), level: s.level }).collect());
    }
    any.then_some(p)
}

/// The edits that carry out the chosen mix_check fixes: one set_mixer per track changed, in the order first touched,
/// with what each fix did; and the fixes left out with why. A fix that can't be carried out changes nothing.
fn plan_mix(p: &Project, fixes: &[MixFix], chosen: &[usize]) -> (Vec<Command>, Vec<Value>, Vec<Value>) {
    let mut work: Vec<(usize, Mixer)> = vec![];
    let (mut applied, mut skipped) = (vec![], vec![]);
    for &i in chosen {
        let Some(f) = fixes.get(i) else {
            skipped.push(json!({"index": i, "why": format!("there is no fix {i}: the check listed {}", fixes.len())}));
            continue;
        };
        let change = serde_json::to_value(&f.change).unwrap_or(Value::Null);
        let ti = match resolve_track(p, &f.track) {
            Ok(ti) => ti,
            Err(why) => {
                skipped.push(json!({"index": i, "track": f.track, "change": change, "why": why}));
                continue;
            }
        };
        let k = match work.iter().position(|w| w.0 == ti) {
            Some(k) => k,
            None => {
                work.push((ti, p.tracks[ti].mixer.clone()));
                work.len() - 1
            }
        };
        let mut trial = work[k].1.clone();
        match mix_change(p, ti, &mut trial, &f.change) {
            Ok(did) => {
                work[k].1 = trial;
                applied.push(json!({"index": i, "track": p.tracks[ti].name, "change": change, "did": did, "code": f.code, "reason": f.reason}));
            }
            Err(why) => skipped.push(json!({"index": i, "track": p.tracks[ti].name, "change": change, "why": why})),
        }
    }
    let cmds = work
        .iter()
        .filter_map(|(ti, after)| {
            let t = &p.tracks[*ti];
            mixer_patch(&t.mixer, after).map(|mixer| Command::SetMixer { track: TrackRef::Id(t.id), mixer })
        })
        .collect();
    (cmds, applied, skipped)
}

/// The band of a split at `crossovers_hz` (the exciter's, the imager's) that holds most of `lo..hi` Hz, by overlap on
/// a log-frequency scale (the band around its centre when it overlaps none).
fn split_band(crossovers_hz: &[f64], lo: f64, hi: f64) -> usize {
    let edges = split_edges(crossovers_hz);
    let (lo, hi) = (lo.max(1.0), hi.max(lo.max(1.0)));
    let mut best = (0, 0.0);
    for (i, &(a, b)) in edges.iter().enumerate() {
        let o = (hi.min(b) / lo.max(a)).ln();
        if o > best.1 + 1e-12 {
            best = (i, o);
        }
    }
    if best.1 > 0.0 {
        return best.0;
    }
    let c = (lo * hi).sqrt();
    edges.iter().position(|&(_, b)| c < b).unwrap_or(edges.len().saturating_sub(1))
}

/// The deepest a move takes a master dynamic EQ band's range (dB).
const DYN_EQ_MAX_DB: f64 = 12.0;
/// The lowest a move takes a master dynamic EQ band's threshold (dB under the band's loud level).
const DYN_EQ_MIN_THRESHOLD_DB: f64 = -12.0;

/// A dynamic EQ move on the master: a new band (from 4 dB under the range's loud level, ratio 3), or the band of the
/// same kind within a third of an octave deepened by `range_db` (to -12 dB), then brought down to catch more.
fn master_dyn_eq(m: &mut Master, kind: EqKind, hz: f64, q: f64, range_db: f64, done: &mut BTreeSet<String>) -> Outcome {
    let hz = hz.clamp(20.0, 20000.0).round();
    let range_db = range_db.clamp(-DYN_EQ_MAX_DB, -0.5);
    let near = m.dyn_eq.iter().position(|b| b.kind == kind && b.channel == Channel::Stereo && (b.hz / hz).log2().abs() <= EQ_MERGE_OCT + 1e-9);
    let at = near.map_or(hz, |i| m.dyn_eq[i].hz);
    claim(done, &format!("dynamic EQ band at {}", hz_text(at)))?;
    let Some(i) = near else {
        let b = DynBand {
            kind,
            hz,
            q: r2(q.clamp(0.1, 10.0)),
            threshold_db: -4.0,
            relative: true,
            ratio: 3.0,
            range_db: r2(range_db),
            attack_ms: 5.0,
            release_ms: 80.0,
            channel: Channel::Stereo,
        };
        m.dyn_eq.push(b);
        return Ok(format!(
            "dynamic EQ: added a {} at {}, Q {:.2}, turning it down by up to {:.1} dB while it's over 4 dB under its loud level",
            kind_name(kind),
            hz_text(hz),
            b.q,
            -b.range_db
        ));
    };
    let b = &mut m.dyn_eq[i];
    let r = r2((b.range_db + range_db).max(-DYN_EQ_MAX_DB));
    if r < b.range_db - 0.05 {
        let old = b.range_db;
        b.range_db = r;
        return Ok(format!("dynamic EQ {} at {}: range {old:.1} -> {r:.1} dB", kind_name(b.kind), hz_text(b.hz)));
    }
    let t = (b.threshold_db - 2.0).max(DYN_EQ_MIN_THRESHOLD_DB);
    if t >= b.threshold_db - 0.05 {
        return Err(format!(
            "the dynamic EQ {} at {} is already at its deepest ({:.0} dB range from {:.0} dB under its loud level): the rest belongs in \
the mix (mix_check's regions name the tracks playing there)",
            kind_name(b.kind),
            hz_text(b.hz),
            b.range_db,
            -b.threshold_db
        ));
    }
    let old = b.threshold_db;
    b.threshold_db = t;
    Ok(format!("dynamic EQ {} at {}: threshold {old:.0} -> {t:.0} dB, so it acts more of the time", kind_name(b.kind), hz_text(b.hz)))
}

/// An exciter move on the master: the band of its split holding most of `hz_lo..hz_hi` blends in at least `mix` of
/// the saturated band on `mode`'s curve. Turned on for the move, the exciter starts with every band at mix 0, so only
/// the band asked for changes the sound.
fn master_exciter(m: &mut Master, hz_lo: f64, hz_hi: f64, mix: f64, mode: Saturation, done: &mut BTreeSet<String>) -> Outcome {
    let added = m.exciter.is_none();
    let e = m.exciter.get_or_insert_with(|| {
        let mut e = Exciter::default();
        e.bands.iter_mut().for_each(|b| b.mix = 0.0);
        e
    });
    if e.bands.len() != e.crossovers_hz.len() + 1 {
        return Err("the exciter's bands don't match its crossovers: set the exciter again".into());
    }
    let i = split_band(&e.crossovers_hz, hz_lo, hz_hi);
    let (lo, hi) = split_edges(&e.crossovers_hz)[i];
    claim(done, &format!("exciter band {} - {}", hz_text(lo), hz_text(hi)))?;
    let b = &mut e.bands[i];
    let mix = r2(mix.clamp(0.0, 1.0));
    if b.mix >= mix - 0.005 {
        return Err(format!(
            "the exciter's {} - {} band already blends in {:.0} %: brighter sources in the mix (or samples recorded at a higher rate) are \
what's left",
            hz_text(lo),
            hz_text(hi),
            b.mix * 100.0
        ));
    }
    let old = b.mix;
    (b.mix, b.mode) = (mix, sat_mode(mode));
    let on = if added { "turned on the exciter (its other bands at mix 0); " } else { "" };
    let mode = if b.mode == SatMode::Tube { "tube" } else { "tape" };
    Ok(format!("{on}exciter {} - {} band: mix {:.0} -> {:.0} %, {mode}", hz_text(lo), hz_text(hi), old * 100.0, mix * 100.0))
}

/// An imager move on the master: the width of the band of its split holding most of `hz_lo..hz_hi` scaled by `width`
/// (0 to 2). Turned on for the move, the imager splits where the report's stereo bands meet, every band at width 1.
fn master_imager(m: &mut Master, hz_lo: f64, hz_hi: f64, width: f64, done: &mut BTreeSet<String>) -> Outcome {
    let added = m.imager.is_none();
    let im = m.imager.get_or_insert_with(|| Imager { crossovers_hz: STEREO_CROSSOVERS_HZ.to_vec(), widths: vec![1.0; STEREO_CROSSOVERS_HZ.len() + 1] });
    if im.widths.len() != im.crossovers_hz.len() + 1 {
        return Err("the imager's widths don't match its crossovers: set the imager again".into());
    }
    let i = split_band(&im.crossovers_hz, hz_lo, hz_hi);
    let (lo, hi) = split_edges(&im.crossovers_hz)[i];
    claim(done, &format!("imager band {} - {}", hz_text(lo), hz_text(hi)))?;
    let old = im.widths[i];
    let new = r2((old * width).clamp(0.0, 2.0));
    if (new - old).abs() < 0.005 {
        return Err(format!(
            "the imager's {} - {} band is already at width {old:.2}: change the tracks playing there instead (their width and pan)",
            hz_text(lo),
            hz_text(hi)
        ));
    }
    im.widths[i] = new;
    let on = if added { "turned on the imager (its other bands at width 1); " } else { "" };
    Ok(format!("{on}imager {} - {} band: width {old:.2} -> {new:.2}", hz_text(lo), hz_text(hi)))
}

/// A high-pass move on the master: a high-pass at `hz` falling `slope` dB per octave added to the master EQ, or the
/// one there raised to `hz` and steepened to `slope` (never lowered or made gentler).
fn master_high_pass(m: &mut Master, hz: f64, slope: u32) -> Outcome {
    let hz = hz.clamp(10.0, 500.0).round();
    let slope = (slope.clamp(12, 48) / 12) * 12;
    match m.eq.iter().position(|b| b.kind == EqKind::HighPass && b.channel == Channel::Stereo) {
        Some(i) => {
            let old = m.eq[i];
            let (h, s) = (old.hz.max(hz), old.slope.max(slope));
            if h <= old.hz && s == old.slope {
                return Err(format!(
                    "the master EQ's high-pass is already at {}, {} dB/octave: what's left down there comes from the tracks (their hpf_hz)",
                    hz_text(old.hz),
                    old.slope
                ));
            }
            (m.eq[i].hz, m.eq[i].slope) = (h, s);
            Ok(format!("EQ high-pass {}, {} dB/octave -> {}, {s} dB/octave", hz_text(old.hz), old.slope, hz_text(h)))
        }
        None => {
            let mut b = EqBand::new(EqKind::HighPass, hz, 0.0, 0.71);
            b.slope = slope;
            m.eq.push(b);
            Ok(format!("EQ: added a high-pass at {}, {slope} dB/octave", hz_text(hz)))
        }
    }
}

/// Claim a master setting for this pass: each is changed once, by the most important move that wants it.
fn claim(done: &mut BTreeSet<String>, key: &str) -> std::result::Result<(), String> {
    if !done.insert(key.to_string()) {
        return Err(format!("another move already changed the master's {key} in this pass: check again to see if this one is still needed"));
    }
    Ok(())
}

/// One master move carried out on a copy of the master. `done` holds the settings already changed in this pass;
/// `delivery` is the loudness the song ships at, and a target move further than 1 LU from it is only taken when
/// `picked` by index.
fn master_move(m: &mut Master, mv: &MasterMove, done: &mut BTreeSet<String>, delivery: f64, picked: bool) -> Outcome {
    match *mv {
        MasterMove::Eq { shape, hz, db, q, part } => {
            let band = EqBand { channel: channel(part), ..EqBand::new(eq_kind(shape), hz, db, q) };
            // each band once a pass: two regions' moves can land on the same band
            let at = near_band(&m.eq, &band).map_or(hz.clamp(20.0, 20000.0), |i| m.eq[i].hz);
            let on = match band.channel {
                Channel::Stereo => "",
                Channel::Mid => "mid ",
                Channel::Side => "side ",
            };
            claim(done, &format!("EQ {on}band at {}", hz_text(at)))?;
            merge_eq(&mut m.eq, band, MASTER_EQ_MAX_DB)
                .map(|d| format!("EQ: {d}"))
                .map_err(|e| format!("{e} on the master: the rest belongs in the mix (mix_check's regions name the tracks playing there)"))
        }
        MasterMove::MonoBelow { hz } => {
            let hz = hz.clamp(0.0, 500.0).round();
            if m.mono_below_hz >= hz - 0.5 {
                return Err(format!("the master is already mono below {}: the wide low end comes from the tracks (their mono_below_hz)", hz_text(m.mono_below_hz)));
            }
            claim(done, "mono_below_hz")?;
            let old = m.mono_below_hz;
            m.mono_below_hz = hz;
            Ok(if old > 0.0 { format!("mono below {} -> {}", hz_text(old), hz_text(hz)) } else { format!("mono below {}", hz_text(hz)) })
        }
        MasterMove::Width { width } => {
            let new = r2((m.width * width).clamp(0.5, 2.0));
            if (new - m.width).abs() < 0.005 {
                return Err(format!("the master's width is already {:.2}: narrow the widest tracks instead (their width or mono_below_hz)", m.width));
            }
            claim(done, "width")?;
            let old = m.width;
            m.width = new;
            Ok(format!("width {old:.2} -> {new:.2}"))
        }
        MasterMove::HighPass { hz, slope } => {
            claim(done, "EQ high-pass")?;
            master_high_pass(m, hz, slope)
        }
        MasterMove::DeEss { hz, range_db } => {
            claim(done, "de-esser")?;
            deess_change(&mut m.deess, hz, range_db, DEESS_THRESHOLD_DB).map_err(|e| {
                format!("the master's {e}: the sibilance comes from the tracks (give the vocals, cymbals or plucks carrying it a de-esser of their own)")
            })
        }
        MasterMove::DynEq { shape, hz, q, range_db } => master_dyn_eq(m, eq_kind(shape), hz, q, range_db, done),
        MasterMove::Exciter { hz_lo, hz_hi, mix, mode } => master_exciter(m, hz_lo, hz_hi, mix, mode, done),
        MasterMove::Imager { hz_lo, hz_hi, width } => master_imager(m, hz_lo, hz_hi, width, done),
        MasterMove::Loudness { target_lufs } => {
            if (target_lufs - m.target_lufs).abs() < 0.05 {
                return Err(format!(
                    "the master already aims at {target_lufs:.1} LUFS and the loudness drive couldn't get there: see master_chain (drive and the \
limiter's gain reduction); a limiter working hard needs a less peaky mix, or a larger limiter clip_db"
                ));
            }
            if !picked && (target_lufs - delivery).abs() > 1.0 + 1e-9 {
                return Err(format!(
                    "{target_lufs:.1} LUFS is {:.1} LU from the {delivery:.1} LUFS delivery target (the game allows 1): pick this issue by index to \
accept it, or ease the glue and the tracks' compressors instead",
                    (target_lufs - delivery).abs()
                ));
            }
            claim(done, "loudness target")?;
            let old = m.target_lufs;
            m.target_lufs = target_lufs;
            Ok(format!("loudness target {old:.1} -> {target_lufs:.1} LUFS"))
        }
        MasterMove::Limiter { release_ms, clip_db } => {
            let mut l = m.limiter;
            let mut did = vec![];
            if let Some(c) = clip_db.map(|c| c.clamp(0.0, 6.0))
                && c > l.clip_db + 0.05
            {
                did.push(format!("clip stage {:.1} -> {c:.1} dB", l.clip_db));
                l.clip_db = c;
            }
            if let Some(r) = release_ms.map(|r| r.clamp(0.0, 2000.0))
                && (r - l.release_ms).abs() > 0.5
            {
                did.push(format!("release {:.0} -> {r:.0} ms", l.release_ms));
                l.release_ms = r;
            }
            if did.is_empty() {
                return Err(format!(
                    "the limiter already clips up to {:.1} dB: if the true peak still goes over, lower ceiling_dbtp by a few tenths or \
lengthen the limiter's lookahead_ms",
                    l.clip_db
                ));
            }
            claim(done, "limiter")?;
            m.limiter = l;
            Ok(format!("limiter: {}", did.join(", ")))
        }
    }
}

/// The master after the chosen issues' moves, with what each did, and the moves left out with why. With `picked`
/// (indices given), an issue without a move is reported; otherwise only issues with moves are considered.
fn plan_master(m: &Master, issues: &[MasterIssue], chosen: &[usize], picked: bool, delivery: f64) -> (Master, Vec<Value>, Vec<Value>) {
    let mut out = m.clone();
    let mut done = BTreeSet::new();
    let (mut applied, mut skipped) = (vec![], vec![]);
    for &i in chosen {
        let Some(issue) = issues.get(i) else {
            skipped.push(json!({"index": i, "why": format!("there is no issue {i}: the check listed {}", issues.len())}));
            continue;
        };
        let Some(mv) = &issue.suggestion else {
            if picked {
                skipped.push(json!({"index": i, "code": issue.code, "why": format!("no master move fixes this: {}", issue.message)}));
            }
            continue;
        };
        let mut trial = out.clone();
        let mut trial_done = done.clone();
        match master_move(&mut trial, mv, &mut trial_done, delivery, picked) {
            Ok(did) => {
                out = trial;
                done = trial_done;
                applied.push(json!({"index": i, "code": issue.code, "move": mv, "did": did}));
            }
            Err(why) => skipped.push(json!({"index": i, "code": issue.code, "move": mv, "why": why})),
        }
    }
    (out, applied, skipped)
}

/// The patch that turns master `a` into `b`, for the settings the moves and presets change; None if nothing did.
fn master_patch(a: &Master, b: &Master) -> Option<MasterPatch> {
    let mut p = MasterPatch::default();
    let mut any = false;
    let mut set = |changed: bool| {
        any |= changed;
        changed
    };
    if set(a.glue != b.glue) {
        p.glue = Some(b.glue);
    }
    if set(a.eq != b.eq) {
        p.eq = Some(b.eq.clone());
    }
    if set(a.eq_phase != b.eq_phase) {
        p.eq_phase = Some(b.eq_phase);
    }
    if set(a.dyn_eq != b.dyn_eq) {
        p.dyn_eq = Some(b.dyn_eq.clone());
    }
    if set(a.deess != b.deess) {
        p.deess = Some(b.deess);
    }
    if set(a.exciter != b.exciter) {
        p.exciter = Some(b.exciter.clone());
    }
    if set(a.imager != b.imager) {
        p.imager = Some(b.imager.clone());
    }
    if set(a.target_lufs != b.target_lufs) {
        p.target_lufs = Some(b.target_lufs);
    }
    if set(a.ceiling_dbtp != b.ceiling_dbtp) {
        p.ceiling_dbtp = Some(b.ceiling_dbtp);
    }
    if set(a.limiter != b.limiter) {
        p.limiter = Some(b.limiter);
    }
    if set(a.multiband != b.multiband) {
        p.multiband = Some(b.multiband.clone());
    }
    if set(a.mono_below_hz != b.mono_below_hz) {
        p.mono_below_hz = Some(b.mono_below_hz);
    }
    if set(a.width != b.width) {
        p.width = Some(b.width);
    }
    any.then_some(p)
}

/// The master presets, by name.
const PRESETS: [&str; 6] = ["transparent", "game_loop", "cinematic", "electronic", "punchy", "acoustic"];

/// A named master chain. `glue` and `mono_below_hz` of None leave the master's as they are.
struct Preset {
    glue: Option<Option<Comp>>,
    multiband: Option<Multiband>,
    limiter: Limiter,
    mono_below_hz: Option<f64>,
}

fn preset(name: &str) -> Result<Preset> {
    let glue = |threshold_db, ratio, attack_ms, release_ms, knee_db, detector, sidechain_hpf_hz, mix| {
        Some(Some(Comp {
            threshold_db,
            relative: true,
            ratio,
            attack_ms,
            release_ms,
            makeup_db: 0.0,
            knee_db,
            detector,
            sidechain_hpf_hz,
            lookahead_ms: 0.0,
            auto_release: true,
            mix,
        }))
    };
    let band = MbBand::new;
    let limiter = |lookahead_ms, release_ms, clip_db| Limiter { lookahead_ms, release_ms, auto_release: true, clip_db };
    Ok(match name {
        "transparent" => Preset { glue: None, multiband: None, limiter: Limiter::default(), mono_below_hz: None },
        "game_loop" => Preset {
            glue: glue(-4.0, 2.0, 30.0, 200.0, 6.0, Detector::Rms, 100.0, 1.0),
            multiband: Some(Multiband::default()),
            limiter: Limiter::default(),
            mono_below_hz: Some(120.0),
        },
        "cinematic" => Preset {
            glue: glue(-3.0, 1.5, 50.0, 400.0, 8.0, Detector::Rms, 80.0, 1.0),
            multiband: Some(Multiband {
                crossovers_hz: vec![120.0, 2000.0, 7000.0],
                bands: vec![band(-5.0, 1.5, 40.0, 300.0), band(-4.0, 1.4, 30.0, 250.0), band(-4.0, 1.5, 15.0, 200.0), band(-5.0, 1.6, 8.0, 150.0)],
            }),
            limiter: limiter(6.0, 150.0, 0.5),
            mono_below_hz: Some(100.0),
        },
        "electronic" => Preset {
            glue: glue(-5.0, 2.5, 10.0, 120.0, 4.0, Detector::Peak, 120.0, 1.0),
            multiband: Some(Multiband {
                crossovers_hz: vec![120.0, 2500.0, 8000.0],
                bands: vec![band(-3.0, 2.5, 20.0, 150.0), band(-3.0, 1.8, 15.0, 120.0), band(-3.0, 2.0, 8.0, 100.0), band(-4.0, 2.0, 3.0, 60.0)],
            }),
            limiter: limiter(4.0, 60.0, 1.5),
            mono_below_hz: Some(150.0),
        },
        "punchy" => Preset {
            glue: glue(-6.0, 3.0, 30.0, 100.0, 3.0, Detector::Peak, 100.0, 0.7),
            multiband: None,
            limiter: limiter(3.0, 50.0, 2.0),
            mono_below_hz: Some(120.0),
        },
        "acoustic" => Preset {
            glue: glue(-2.0, 1.5, 40.0, 300.0, 10.0, Detector::Rms, 80.0, 1.0),
            multiband: None,
            limiter: limiter(6.0, 200.0, 0.5),
            mono_below_hz: None,
        },
        _ => bail!("unknown preset '{name}': give {}", PRESETS.join(", ")),
    })
}

impl Preset {
    fn apply_to(&self, m: &mut Master) {
        if let Some(g) = self.glue {
            m.glue = g;
        }
        m.multiband = self.multiband.clone();
        m.limiter = self.limiter;
        if let Some(hz) = self.mono_below_hz {
            m.mono_below_hz = hz;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// master

/// The master tool's options that change what a check finds.
struct MasterArgs {
    style: TonalStyle,
    reference: Option<PathBuf>,
    target_lufs: Option<f64>,
    ceiling_dbtp: Option<f64>,
}

impl MasterArgs {
    fn of(a: &Value) -> Result<MasterArgs> {
        let style = get::<String>(a, "style")?.map(|s| s.parse::<TonalStyle>()).transpose()?.unwrap_or_default();
        let reference = get::<String>(a, "reference")?.map(|r| paths::resolve(&r));
        if let Some(r) = &reference {
            ensure!(r.is_file(), "the reference {} does not exist: give a WAV or FLAC file (keep them in refs/)", rel(r));
        }
        let target_lufs = get::<f64>(a, "target_lufs")?;
        if let Some(t) = target_lufs {
            ensure!((-40.0..=0.0).contains(&t), "target_lufs is {t}: give -40 to 0 LUFS (Roblox: -15)");
        }
        let ceiling_dbtp = get::<f64>(a, "ceiling_dbtp")?;
        if let Some(c) = ceiling_dbtp {
            ensure!((-20.0..=0.0).contains(&c), "ceiling_dbtp is {c}: give -20 to 0 dBTP (Roblox: -1)");
        }
        Ok(MasterArgs { style, reference, target_lufs, ceiling_dbtp })
    }

    /// Everything a check depends on besides the render, the reference by its path, size and time.
    fn key(&self) -> String {
        let reference = self.reference.as_ref().map(|r| {
            let meta = std::fs::metadata(r).ok();
            format!("{}:{:?}:{:?}", r.display(), meta.as_ref().map(|m| m.len()), meta.and_then(|m| m.modified().ok()))
        });
        format!("{}|{:?}|{:?}|{reference:?}", self.style.name(), self.target_lufs, self.ceiling_dbtp)
    }

    fn reference_audio(&self) -> Result<Option<Audio>> {
        self.reference.as_ref().map(|r| Audio::read(r).with_context(|| format!("reading the reference {}", rel(r)))).transpose()
    }

    /// The master with the given target and ceiling set on it.
    fn targets_on(&self, m: &mut Master) {
        if let Some(t) = self.target_lufs {
            m.target_lufs = t;
        }
        if let Some(c) = self.ceiling_dbtp {
            m.ceiling_dbtp = c;
        }
    }
}

/// The last master check: the options it was made with, the render it measured (held weakly, so an old render's
/// audio is not kept alive for it) and the report. Indices picked from it stay valid while the options and that
/// render are the project's current ones.
struct MasterCheck {
    opts: String,
    render: Weak<LastRender>,
    report: Arc<MasterReport>,
}

impl MasterCheck {
    fn measured(&self, r: &Arc<LastRender>) -> bool {
        self.render.upgrade().is_some_and(|x| Arc::ptr_eq(&x, r))
    }
}

fn last_master_check() -> &'static Mutex<Option<MasterCheck>> {
    static C: OnceLock<Mutex<Option<MasterCheck>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// The last check, if it was made with `opts` of the project as it is now.
fn current_check(sh: &Arc<Shared>, opts: &str) -> Option<Arc<MasterReport>> {
    let now = {
        let st = sh.lock();
        st.render.as_ref().filter(|r| r.revision == st.session.revision).cloned()
    }?;
    last_master_check().lock().as_ref().filter(|c| c.opts == opts && c.measured(&now)).map(|c| c.report.clone())
}

/// Whether the project has more than one stem layer (then the master is checked with its layer stems).
fn has_layers(p: &Project) -> bool {
    p.tracks.iter().any(|t| t.layer_name() != "base")
}

/// The render of the current revision (with layer stems when asked), rendered now if stale.
fn master_render(sh: &Arc<Shared>, layers: bool) -> Result<Arc<LastRender>> {
    let fresh = {
        let st = sh.lock();
        st.render.as_ref().filter(|r| r.revision == st.session.revision && (!layers || !r.out.layers.is_empty())).cloned()
    };
    if let Some(r) = fresh {
        return Ok(r);
    }
    ensure_render_with(sh, 900.0, layers)?;
    sh.lock().render.clone().ok_or_else(|| anyhow!("nothing rendered"))
}

/// A render's layer stems in stacking order: base first, then as the tracks first use them.
fn stacked_layers<'a>(p: &Project, out: &'a RenderOutput) -> Vec<(String, AudioRef<'a>)> {
    let mut order: Vec<&str> = vec!["base"];
    for t in &p.tracks {
        if !order.contains(&t.layer_name()) {
            order.push(t.layer_name());
        }
    }
    let rank = |name: &str| order.iter().position(|o| *o == name).unwrap_or(order.len());
    let mut v: Vec<(String, AudioRef)> = out.layers.iter().map(|(name, buf)| (name.clone(), view(out.sr, buf))).collect();
    v.sort_by_key(|(name, _)| rank(name));
    v
}

fn run_master_check(r: &LastRender, args: &MasterArgs, reference: Option<&Audio>) -> Result<MasterReport> {
    let out = &r.out;
    let n = out.left.len().min(out.right.len());
    let layers = if out.layers.len() > 1 { stacked_layers(&r.project, out) } else { vec![] };
    let o = MasterOptions {
        target_lufs: args.target_lufs.unwrap_or(r.project.master.target_lufs),
        ceiling_dbtp: args.ceiling_dbtp.unwrap_or(r.project.master.ceiling_dbtp),
        looped: out.looped,
        bar_starts: out.bar_starts.clone(),
        layers,
        reference: reference.map(Audio::view),
        style: args.style,
        sections: out.sections.iter().filter(|s| s.start < s.end && s.start < n).map(|s| (s.name.clone(), s.start, s.end.min(n))).collect(),
    };
    check_master(AudioRef::master(out), &o)
}

/// The agent's last master check, if it measured render `r`: what the GUI's analysis shows first.
pub fn checked(r: &Arc<LastRender>) -> Option<Arc<MasterReport>> {
    last_master_check().lock().as_ref().filter(|c| c.measured(r)).map(|c| c.report.clone())
}

/// Render `r` checked against `style`'s masters at the project's own target and ceiling: the GUI's analysis, where
/// the agent hasn't checked it.
pub fn check_for_gui(r: &LastRender, style: TonalStyle) -> Result<MasterReport> {
    run_master_check(r, &MasterArgs { style, reference: None, target_lufs: None, ceiling_dbtp: None }, None)
}

/// The current revision rendered and checked (the check reused while the render and the options are the same).
fn master_report(sh: &Arc<Shared>, args: &MasterArgs) -> Result<(Arc<LastRender>, Arc<MasterReport>)> {
    let layers = has_layers(&sh.lock().session.project);
    let r = master_render(sh, layers)?;
    let opts = args.key();
    if let Some(c) = last_master_check().lock().as_ref()
        && c.opts == opts
        && c.measured(&r)
    {
        return Ok((r, c.report.clone()));
    }
    let reference = args.reference_audio()?;
    let report = Arc::new(run_master_check(&r, args, reference.as_ref())?);
    *last_master_check().lock() = Some(MasterCheck { opts, render: Arc::downgrade(&r), report: report.clone() });
    Ok((r, report))
}

fn issues_json(issues: &[MasterIssue]) -> Value {
    json!(issues
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let mut o = json!({"index": i, "code": x.code, "severity": x.severity, "message": x.message});
            if let Some(m) = &x.suggestion {
                o["move"] = json!(m);
            }
            o
        })
        .collect::<Vec<_>>())
}

/// A check in a reply: compact (the headline numbers, what is out of tolerance, the issues, the chain's levels) or
/// the whole report.
fn check_json(rep: &MasterReport, r: &LastRender, full: bool) -> Result<Value> {
    let mut v = if full {
        serde_json::to_value(rep)?
    } else {
        let l = &rep.loudness;
        let mut o = json!({
            "score": rep.score, "summary": rep.summary,
            "loudness": {"integrated_lufs": l.integrated_lufs, "target_lufs": l.target_lufs, "from_target_lu": l.from_target_lu,
                "true_peak_dbtp": l.true_peak_dbtp, "ceiling_dbtp": l.ceiling_dbtp, "short_term_max_lufs": l.short_term_max_lufs,
                "lra_lu": l.lra_lu, "plr_db": l.plr_db, "psr_min_db": l.psr_min_db, "clipped_samples": l.clipped_samples},
            "stereo": {"correlation": rep.stereo.correlation, "fold_down_lu": rep.stereo.fold_down_lu, "low_end_side_db": rep.stereo.low_end_side_db},
            "dynamics": {"plr": rep.dynamics.plr_verdict, "lra": rep.dynamics.lra_verdict, "squashed_transients": rep.dynamics.squashed_transients},
        });
        if let Some(t) = &rep.tonal {
            let outside: Vec<Value> = t
                .regions
                .iter()
                .filter(|x| x.code.is_some())
                .map(|x| json!({"region": x.region, "hz": format!("{:.0}-{:.0}", x.lo_hz, x.hi_hz), "deviation_db": x.deviation_db, "tolerance_db": x.tolerance_db, "verdict": x.verdict}))
                .collect();
            o["tonal"] = json!({"target": t.target, "deviation_rms_db": t.deviation_rms_db, "outside": outside,
                "closest_style": t.closest_style, "styles": t.styles});
        }
        if let Some(ls) = &rep.layers {
            o["layers"] = json!({"sums_to_master": ls.sums_to_master, "layers": ls.layers.iter().map(|x| json!({"name": x.name, "lufs": x.lufs,
                "adds_lu": x.adds_lu, "stacked_true_peak_dbtp": x.stacked_true_peak_dbtp, "over_ceiling": x.over_ceiling})).collect::<Vec<_>>()});
        }
        o
    };
    v["issues"] = issues_json(&rep.issues);
    v["chain"] = master_chain_json(&r.project, &r.out);
    v["revision"] = json!(r.revision);
    Ok(v)
}

/// What to do after a check.
fn master_next(rep: &MasterReport) -> &'static str {
    if rep.issues.iter().any(|i| i.suggestion.is_some() && i.severity != "info") {
        "master action=apply carries out the moves (all, or apply=[indices]) and checks again; action=auto repeats that until the score stops rising"
    } else if rep.issues.iter().any(|i| i.severity != "info") {
        "what is left has no master move: fix it in the mix (mix_check names the tracks), then check again"
    } else {
        "ready to deliver: export it, then game_audio_check judges it beside the game's other music"
    }
}

fn master(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let args = MasterArgs::of(a)?;
    let detailed = full(a)?;
    match get::<String>(a, "action")?.as_deref().unwrap_or("check") {
        "check" => {
            let (r, rep) = master_report(sh, &args)?;
            let mut v = check_json(&rep, &r, detailed)?;
            v["next"] = json!(master_next(&rep));
            reply(v)
        }
        "apply" => master_apply(sh, a, &args, detailed),
        "match" => master_match(sh, a, &args),
        "auto" => master_auto(sh, a, &args, detailed),
        "preset" => master_preset(sh, a, &args, detailed),
        other => bail!("unknown action '{other}': check, apply, match, auto or preset"),
    }
}

/// Carry out moves of `rep` (the check of render `r`) on the master as one undo step, with the master tool's target
/// and ceiling: what was applied and skipped, and the revision it made (None if nothing changed). Refused if the
/// project changed since `r` was rendered.
fn apply_moves(sh: &Arc<Shared>, args: &MasterArgs, r: &LastRender, rep: &MasterReport, chosen: Option<&[usize]>, label: &str) -> Result<(Vec<Value>, Vec<Value>, Option<u64>)> {
    sh.with_session(|ses| -> Result<_> {
        ensure!(ses.revision == r.revision, "the project changed while it was checked (revision {} is now {}): check again", r.revision, ses.revision);
        let mut base = ses.project.master.clone();
        args.targets_on(&mut base);
        let all: Vec<usize> = (0..rep.issues.len()).collect();
        let (picked, list) = match chosen {
            Some(v) => (true, Pick::These(v.to_vec()).indices(rep.issues.len())),
            None => (false, all),
        };
        let (after, applied, skipped) = plan_master(&base, &rep.issues, &list, picked, base.target_lufs);
        let Some(patch) = master_patch(&ses.project.master, &after) else { return Ok((applied, skipped, None)) };
        let codes: Vec<&str> = applied.iter().filter_map(|x| x["code"].as_str()).collect();
        let label = if codes.is_empty() { label.to_string() } else { format!("{label}: {}", codes.join(", ")) };
        ses.apply_batch_labeled(&[Command::SetMaster { master: patch }], flux_core::Source::Agent, Some(&label))?;
        Ok((applied, skipped, Some(ses.revision)))
    })
}

fn master_apply(sh: &Arc<Shared>, a: &Value, args: &MasterArgs, detailed: bool) -> Result<Vec<Content>> {
    let chosen = get::<Vec<usize>>(a, "apply")?;
    if chosen.is_some() {
        ensure!(
            current_check(sh, &args.key()).is_some(),
            "the indices must come from a check of the project as it is now (an edit or another render came since the last one), with \
the same style, reference and targets: run master check and pick from its issues, or leave out `apply` to carry out every move"
        );
    }
    let (r, rep) = master_report(sh, args)?;
    let (applied, skipped, revision) = apply_moves(sh, args, &r, &rep, chosen.as_deref(), "master apply")?;
    if revision.is_none() {
        let mut v = json!({"applied": applied, "skipped": skipped, "score": rep.score, "note": "nothing was changed"});
        v["next"] = json!(master_next(&rep));
        return reply(v);
    }
    let (r2, rep2) = master_report(sh, args)?;
    let mut v = json!({
        "applied": applied, "skipped": skipped, "revision": revision,
        "before": {"score": rep.score, "summary": rep.summary}, "after": {"score": rep2.score, "summary": rep2.summary},
        "check": check_json(&rep2, &r2, detailed)?,
    });
    v["next"] = json!(if rep2.score < rep.score {
        "the score dropped: undo reverts this step (one undo), then try the moves one at a time with apply=[index]"
    } else {
        master_next(&rep2)
    });
    reply(v)
}

fn master_auto(sh: &Arc<Shared>, a: &Value, args: &MasterArgs, detailed: bool) -> Result<Vec<Content>> {
    let passes = get::<u32>(a, "passes")?.unwrap_or(3).clamp(1, 5);
    let (mut r, mut rep) = master_report(sh, args)?;
    let start = rep.score;
    let mut trail = vec![];
    let mut stopped = format!("ran all {passes} passes");
    for pass in 1..=passes {
        let (applied, skipped, revision) = apply_moves(sh, args, &r, &rep, None, &format!("master auto pass {pass}"))?;
        let Some(revision) = revision else {
            trail.push(json!({"pass": pass, "score": rep.score, "applied": applied, "skipped": skipped}));
            stopped = "no move left that can be applied".into();
            break;
        };
        let (r2, rep2) = master_report(sh, args)?;
        let (before, after) = (rep.score, rep2.score);
        if after < before {
            let undone = sh.with_session(|s| s.revision == revision && s.undo(flux_core::Source::Agent));
            trail.push(json!({"pass": pass, "score_before": before, "score_after": after, "applied": applied, "skipped": skipped, "undone": undone}));
            stopped = if undone {
                format!("pass {pass} lowered the score ({before} -> {after}), so it was undone")
            } else {
                format!("pass {pass} lowered the score ({before} -> {after}) but the project changed since, so it was kept: undo it by hand if it should go")
            };
            if !undone {
                (r, rep) = (r2, rep2);
            }
            break;
        }
        trail.push(json!({"pass": pass, "score_before": before, "score_after": after, "applied": applied, "skipped": skipped}));
        (r, rep) = (r2, rep2);
        if after < before + 1 {
            stopped = format!("pass {pass} gained under 1 point");
            break;
        }
    }
    let revision = sh.lock().session.revision;
    let mut v = json!({"score_before": start, "score_after": rep.score, "passes": trail, "stopped": stopped, "revision": revision,
        "check": check_json(&rep, &r, detailed)?});
    v["next"] = json!(master_next(&rep));
    reply(v)
}

fn master_match(sh: &Arc<Shared>, a: &Value, args: &MasterArgs) -> Result<Vec<Content>> {
    let Some(path) = &args.reference else {
        bail!("match needs a reference: a WAV or FLAC of a finished track whose tonal balance this song should have (keep them in refs/)");
    };
    let amount = get::<f64>(a, "amount")?.unwrap_or(0.6);
    ensure!(amount > 0.0 && amount <= 1.0, "amount is {amount}: give more than 0 and at most 1 (0.6 matches most of the way)");
    let reference = args.reference_audio()?.ok_or_else(|| anyhow!("no reference"))?;
    let refspec = LongTermSpectrum::new(reference.view()).context("the reference")?;
    let r = master_render(sh, false)?;
    let mix = LongTermSpectrum::new(AudioRef::master(&r.out)).context("the master")?;
    let fit = match_eq_spectra(&mix, &refspec, &MatchOptions { amount, ..Default::default() })?;
    // how far apart they are: the whole difference's RMS, the measure match_eq fits
    let deviation = |m: &LongTermSpectrum| -> Result<f64> {
        Ok(match_eq_spectra(m, &refspec, &MatchOptions { amount: 1.0, max_bands: 1, ..Default::default() })?.rms_before_db)
    };
    let before = deviation(&mix)?;
    let bands: Vec<Value> = fit.bands.iter().map(|b| json!(b)).collect();
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (did, left, revision) = sh.with_session(|ses| -> Result<_> {
        ensure!(ses.revision == r.revision, "the project changed while it was measured (revision {} is now {}): match again", r.revision, ses.revision);
        let mut m = ses.project.master.clone();
        args.targets_on(&mut m);
        // the fitted bands work together on top of the EQ the master was measured with: each folds into a band that
        // was there before, never into another of the fit
        let (mut did, mut left, mut fresh) = (vec![], vec![], vec![]);
        for b in &fit.bands {
            let band = EqBand::new(eq_kind(b.shape), b.hz, b.db, b.q);
            if near_band(&m.eq, &band).is_none() {
                did.push(add_band(&mut fresh, band, MASTER_EQ_MAX_DB));
                continue;
            }
            match merge_eq(&mut m.eq, band, MASTER_EQ_MAX_DB) {
                Ok(d) => did.push(d),
                Err(e) => left.push(format!("{e} on the master")),
            }
        }
        m.eq.extend(fresh);
        let Some(patch) = master_patch(&ses.project.master, &m) else { return Ok((did, left, None)) };
        let label = format!("master match: {} EQ change{} toward {name}", did.len(), if did.len() == 1 { "" } else { "s" });
        ses.apply_batch_labeled(&[Command::SetMaster { master: patch }], flux_core::Source::Agent, Some(&label))?;
        Ok((did, left, Some(ses.revision)))
    })?;
    let Some(revision) = revision else {
        return reply(json!({"bands": bands, "left_out": left, "deviation_db": before,
            "note": "the master already matches the reference as closely as its EQ goes: nothing was changed. What is left belongs in the mix"}));
    };
    let r2 = master_render(sh, false)?;
    let after = deviation(&LongTermSpectrum::new(AudioRef::master(&r2.out)).context("the master")?)?;
    let mut v = json!({
        "bands": bands, "did": did, "amount": amount, "level_offset_db": fit.offset_db, "revision": revision,
        "deviation_before_db": before, "deviation_after_db": after,
        "note": "deviation: the RMS over 30 Hz-16 kHz of the tonal difference from the reference at equal loudness",
        "next": "master check with this reference shows what is left; undo reverts the match",
    });
    if !left.is_empty() {
        v["left_out"] = json!(left);
    }
    reply(v)
}

fn master_preset(sh: &Arc<Shared>, a: &Value, args: &MasterArgs, detailed: bool) -> Result<Vec<Content>> {
    let name = get::<String>(a, "name")?.ok_or_else(|| anyhow!("preset needs a name: {}", PRESETS.join(", ")))?;
    let p = preset(&name)?;
    let before = current_check(sh, &args.key()).map(|c| c.score);
    let (set, revision) = sh.with_session(|ses| -> Result<_> {
        let mut m = ses.project.master.clone();
        p.apply_to(&mut m);
        args.targets_on(&mut m);
        let set = master_diff(&ses.project.master, &m);
        let Some(patch) = master_patch(&ses.project.master, &m) else { return Ok((set, None)) };
        ses.apply_batch_labeled(&[Command::SetMaster { master: patch }], flux_core::Source::Agent, Some(&format!("master preset {name}")))?;
        Ok((set, Some(ses.revision)))
    })?;
    let (r, rep) = master_report(sh, args)?;
    let mut v = json!({"preset": name, "revision": revision, "score_before": before, "score": rep.score, "check": check_json(&rep, &r, detailed)?});
    match revision {
        Some(_) => v["set"] = set,
        None => v["note"] = json!("the master already has this preset's settings: nothing was changed"),
    }
    v["next"] = json!(master_next(&rep));
    reply(v)
}

/// The master's settings that differ between `a` and `b`, with b's values (null: turned off).
fn master_diff(a: &Master, b: &Master) -> Value {
    let (Ok(Value::Object(x)), Ok(Value::Object(y))) = (serde_json::to_value(a), serde_json::to_value(b)) else { return Value::Null };
    let mut o = serde_json::Map::new();
    for k in x.keys().chain(y.keys()) {
        if x.get(k) != y.get(k) && !o.contains_key(k) {
            o.insert(k.clone(), y.get(k).cloned().unwrap_or(Value::Null));
        }
    }
    Value::Object(o)
}

// ---------------------------------------------------------------------------------------------
// game_audio_check

/// What the game ships from `output/`: per song folder, FLAC files, WAVs without a FLAC of the same name beside
/// them, and `.layers.json` sets. `stems/` folders hold the pipeline's intermediate stems and are skipped.
fn deliverables(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(dir).with_context(|| format!("listing {}", rel(dir)))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if p.is_dir() {
            if depth < 2 && name != "stems" {
                deliverables(&p, depth + 1, out)?;
            }
        } else if name.ends_with(".layers.json") || name.ends_with(".flac") || (name.ends_with(".wav") && !p.with_extension("flac").exists()) {
            out.push(p);
        }
    }
    Ok(())
}

fn game_audio_check(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let mut o = GameOptions { detect_loops: get::<bool>(a, "detect_loops")?.unwrap_or(true), ..Default::default() };
    if let Some(v) = get::<f64>(a, "target_lufs")? {
        o.target_lufs = v;
    }
    if let Some(v) = get::<f64>(a, "ceiling_dbtp")? {
        o.ceiling_dbtp = v;
    }
    if let Some(v) = get::<u64>(a, "max_file_bytes")? {
        o.max_file_bytes = v;
    }
    let files = match get::<Vec<String>>(a, "paths")? {
        Some(p) => {
            o.recursive = get::<bool>(a, "recursive")?.unwrap_or(false);
            p.iter().map(|s| paths::resolve(s)).collect()
        }
        None => {
            let mut found = vec![];
            let dir = paths::exports_dir();
            if dir.is_dir() {
                deliverables(&dir, 0, &mut found)?;
            }
            found
        }
    };
    let project = if get::<bool>(a, "include_project")?.unwrap_or(false) { Some(current_render(sh)?) } else { None };
    ensure!(!files.is_empty() || project.is_some(), "nothing to check: output/ has no deliverables (give paths, or include_project=true)");
    let audio: Vec<GameInput> = project.iter().map(|r| GameInput::from_render(format!("{} (open project)", r.project.name), &r.out)).collect();
    let report = check_game_with(&files, audio, &o)?;
    let mut v = serde_json::to_value(&report)?;
    let detailed = full(a)?;
    for t in v["tracks"].as_array_mut().into_iter().flatten() {
        if let Some(p) = t["path"].as_str() {
            t["path"] = json!(rel(Path::new(p)));
        }
        if !detailed && let Some(m) = t.as_object_mut() {
            let s = &m["stats"];
            let stats = json!({"integrated_lufs": s["integrated_lufs"], "true_peak_dbtp": s["true_peak_dbtp"], "lra_lu": s["lra_lu"],
                "short_term_max_lufs": s["short_term_max_lufs"], "plr_db": s["plr_db"]});
            m.insert("stats".into(), stats);
            m.remove("seam");
            m.remove("layers");
            m.remove("bits");
        }
    }
    for u in v["unreadable"].as_array_mut().into_iter().flatten() {
        if let Some(p) = u["path"].as_str() {
            u["path"] = json!(rel(Path::new(p)));
        }
    }
    reply(v)
}

// ---------------------------------------------------------------------------------------------
// spectrogram

enum Source {
    Master,
    Dry,
    Track(String),
    File(String),
}

fn spectrogram(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let names = get::<Vec<String>>(a, "sources")?.unwrap_or_else(|| vec!["master".into()]);
    ensure!(!names.is_empty() && names.len() <= 8, "give 1 to 8 sources");
    let sources: Vec<Source> = names
        .iter()
        .map(|n| match n.to_ascii_lowercase().as_str() {
            "master" => Source::Master,
            "dry" => Source::Dry,
            _ if is_audio(Path::new(n)) => Source::File(n.clone()),
            _ => Source::Track(n.clone()),
        })
        .collect();
    let needs_tracks = sources.iter().any(|s| matches!(s, Source::Dry | Source::Track(_)));
    let needs_render = sources.iter().any(|s| !matches!(s, Source::File(_)));
    let sep = if needs_tracks { Some(separated(sh)?) } else { None };
    let main = if needs_render && sep.is_none() { Some(current_render(sh)?) } else { None };
    let rendered: Option<(&RenderOutput, &Project)> =
        sep.as_ref().map(|s| (&s.out, &s.project)).or(main.as_ref().map(|r| (&r.out, &r.project)));
    let files: Vec<Option<Audio>> =
        sources.iter().map(|s| if let Source::File(f) = s { read_audio(f).map(|x| Some(x.1)) } else { Ok(None) }).collect::<Result<_>>()?;

    let mut bands: Vec<AudioRef> = vec![];
    for (s, file) in sources.iter().zip(&files) {
        let band = match (s, rendered) {
            (Source::File(_), _) => file.as_ref().map(Audio::view).expect("decoded above"),
            (Source::Master, Some((out, _))) => AudioRef::master(out),
            (Source::Dry, Some((out, _))) => view(out.sr, out.dry.as_ref().ok_or_else(|| anyhow!("the render has no dry master"))?),
            (Source::Track(name), Some((out, p))) => match out.track_stems.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                Some((_, buf)) => view(out.sr, buf),
                None if p.tracks.iter().any(|t| t.name.eq_ignore_ascii_case(name)) => bail!("track '{name}' is muted or silent in this render"),
                None => bail!("'{name}' is not master, dry, a track or a .wav/.flac file"),
            },
            _ => unreachable!("rendered when a source needs it"),
        };
        bands.push(band);
    }

    let longest = bands.iter().map(|b| b.duration_s()).fold(0.0, f64::max);
    let (t0, t1) = match (get::<u32>(a, "from_bar")?, get::<u32>(a, "to_bar")?) {
        (None, None) => (get::<f64>(a, "start_s")?.unwrap_or(0.0), get::<f64>(a, "end_s")?.unwrap_or(longest)),
        (from, to) => {
            let session;
            let p = match rendered {
                Some((_, p)) => p,
                None => {
                    session = sh.lock().session.project.clone();
                    &session
                }
            };
            let tm = TimeMap::new(p);
            let from = from.unwrap_or(1);
            let to = to.unwrap_or(p.bars).max(from);
            (tm.sec(tm.bar_beat(from)), tm.sec(tm.bar_beat(to + 1)))
        }
    };
    let t1 = t1.min(longest);
    ensure!(t0 >= 0.0 && t1 > t0, "the time range {t0:.2}-{t1:.2} s is empty (the audio is {longest:.2} s long)");
    let o = SpectroOptions {
        width: get::<usize>(a, "width")?.unwrap_or(1200).clamp(100, 4000),
        height: get::<usize>(a, "height")?.unwrap_or(160).clamp(40, 600),
        ..Default::default()
    };
    let png = flux_analysis::spectro::spectrogram(&bands, t0, t1, &o)?.encode_png()?;
    // a black band is a silent source, not a failure: say so with numbers
    let peaks: Vec<Value> = bands
        .iter()
        .map(|b| {
            let s = b.slice_s(t0, t1);
            let peak = s.left.iter().chain(s.right).fold(0f32, |m, x| m.max(x.abs()));
            if peak > SILENT { json!(20.0 * (peak as f64).log10()) } else { json!("silent") }
        })
        .collect();
    let mut info = json!({"bands_top_to_bottom": names, "peak_dbfs": peaks, "start_s": t0, "end_s": t1,
        "axes": "x: time, a blue line every second; y: log frequency per band, 30 Hz (bottom) to 16 kHz (top), dotted lines at \
50, 100, 200, 500 Hz, 1, 2, 5 and 10 kHz; brighter = louder, 80 dB range; a white line separates the bands"});
    if let Some(s) = get::<String>(a, "save")? {
        let path = paths::resolve(&s);
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&path, &png).with_context(|| format!("writing {}", rel(&path)))?;
        info["saved"] = json!(rel(&path));
    }
    round2(&mut info);
    Ok(vec![Content::Text(serde_json::to_string(&info)?), Content::Png(png)])
}

// ---------------------------------------------------------------------------------------------
// instrument_card

/// A card at least as new as its SFZ.
fn card_is_fresh(card: &Path, sfz: &Path) -> bool {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    matches!((modified(card), modified(sfz)), (Some(c), Some(s)) if c >= s)
}

fn instrument_card(a: &Value) -> Result<Vec<Content>> {
    let files: Vec<PathBuf> = req::<Vec<String>>(a, "sfz")?.iter().map(|s| paths::resolve(s)).collect();
    ensure!(!files.is_empty(), "give at least one SFZ");
    for f in &files {
        ensure!(f.is_file(), "{} does not exist", rel(f));
    }
    let copts = CardOptions {
        keys: get::<String>(a, "keys")?,
        ks: get::<PitchSpec>(a, "ks")?.map(|p| p.resolve()).transpose()?.map(i64::from),
        ..Default::default()
    };
    copts.key_range()?;
    let refresh = get::<bool>(a, "refresh")?.unwrap_or(false);
    let out = if get::<bool>(a, "save")?.unwrap_or(true) { paths::root().join("catalog") } else { std::env::temp_dir().join("flux-cards") };
    let jobs = catalog::jobs_for(&files, get::<String>(a, "name")?.as_deref(), &copts, &out)?;
    let mut cards: Vec<Option<(flux_analysis::card::Card, bool)>> = jobs
        .iter()
        .map(|j| {
            let path = out.join(format!("{}.json", j.slug));
            if refresh || !card_is_fresh(&path, &j.path) {
                return None;
            }
            catalog::read_card(&path).ok().filter(|c| c.error.is_none()).map(|c| (c, true))
        })
        .collect();
    let todo: Vec<catalog::Job> = jobs.iter().zip(&cards).filter(|(_, c)| c.is_none()).map(|(j, _)| j.clone()).collect();
    if !todo.is_empty() {
        // each card in its own process (`flux card-worker`), so a crashing instrument can't take Flux down
        let isolation = Isolation::Subprocess { exe: std::env::current_exe()?, args: vec!["card-worker".into()] };
        let mut made = catalog::card_many(&todo, &out, &RunOptions::new(copts, isolation), &|_| {})?.into_iter();
        for c in cards.iter_mut().filter(|c| c.is_none()) {
            *c = made.next().map(|card| (card, false));
        }
    }
    let detailed = full(a)?;
    let list: Vec<Value> = cards
        .into_iter()
        .zip(&jobs)
        .map(|(c, j)| {
            let Some((card, cached)) = c else { return Ok(json!({"sfz": rel(&j.path), "error": "no card was made"})) };
            let mut v = serde_json::to_value(&card)?;
            if let Some(m) = v.as_object_mut() {
                if !detailed {
                    m.remove("per_key");
                }
                m.insert("cached".into(), json!(cached));
                m.insert("card".into(), json!(rel(&out.join(format!("{}.json", j.slug)))));
            }
            Ok(v)
        })
        .collect::<Result<_>>()?;
    reply(json!({"cards": list}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::Session;

    fn project(cmds: Value) -> Project {
        let mut p = Project::new("Test", 120.0, 4);
        for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
            flux_core::commands::apply(&mut p, &c).unwrap();
        }
        p
    }

    fn peak(hz: f64, db: f64, q: f64) -> EqBand {
        EqBand::new(EqKind::Peak, hz, db, q)
    }

    fn fix(track: &str, change: MixChange) -> MixFix {
        MixFix { track: track.into(), change, reason: "test".into(), code: "masking".into() }
    }

    fn issue(code: &str, mv: Option<MasterMove>) -> MasterIssue {
        MasterIssue { code: code.into(), severity: "warn", message: format!("{code} found"), suggestion: mv }
    }

    fn indices(v: &[Value]) -> Vec<u64> {
        v.iter().map(|x| x["index"].as_u64().unwrap()).collect()
    }

    /// Whether a master passes flux-core's validation.
    fn valid(from: &Master, to: &Master) -> bool {
        let mut p = Project::new("Test", 120.0, 4);
        p.master = from.clone();
        let patch = master_patch(from, to).expect("something changed");
        flux_core::commands::apply(&mut p, &Command::SetMaster { master: patch }).is_ok() && p.master == *to
    }

    /// A pad sending to a reverb return and a delay return, with a reverb send and a delay of its own.
    fn routed() -> Project {
        project(json!([
            {"op": "add_track", "name": "Pad", "instrument": {"kind": "none"}},
            {"op": "add_track", "name": "Dry", "instrument": {"kind": "none"}},
            {"op": "add_bus", "name": "Verb", "sends": [{"track": "Pad", "level": 0.4}], "mixer": {"reverb": {"room_size": 0.7,
                "damping": 0.5, "predelay_ms": 20, "hpf_hz": 200, "lpf_hz": 8000, "return_db": 0, "mix": 1}}},
            {"op": "add_bus", "name": "Echo", "sends": [{"track": "Pad", "level": 0.5}], "mixer": {"delay": {"seconds": 0.375,
                "feedback": 0.3, "mix": 1}}},
            {"op": "set_mixer", "track": "Pad", "mixer": {"reverb_send": 0.2, "hpf_hz": 40, "delay": {"seconds": 0.25, "feedback": 0.2, "mix": 0.4}}},
        ]))
    }

    #[test]
    fn eq_bands_merge_within_a_third_of_an_octave_and_clamp() {
        let mut eq = vec![];
        let merge = |eq: &mut Vec<EqBand>, b: EqBand| merge_eq(eq, b, EQ_MAX_DB).unwrap_or_else(|e| panic!("{b:?}: {e}"));
        merge(&mut eq, peak(300.0, -3.0, 1.0));
        merge(&mut eq, peak(350.0, -2.0, 2.0));
        assert_eq!(eq, [peak(300.0, -5.0, 1.0)], "a band 0.22 octave away merges, keeping the first's frequency and Q");
        merge(&mut eq, peak(400.0, -2.0, 1.0));
        merge(&mut eq, EqBand::new(EqKind::LowShelf, 300.0, 2.0, 0.7));
        assert_eq!(eq.len(), 3, "0.42 octave away, or another kind, is a band of its own");
        merge(&mut eq, peak(290.0, -10.0, 1.0));
        assert_eq!(eq[0].db, -12.0);
        let at_limit = eq.clone();
        assert!(merge_eq(&mut eq, peak(300.0, -1.0, 1.0), EQ_MAX_DB).unwrap_err().contains("already at -12.0 dB"));
        assert_eq!(eq, at_limit);
        let did = merge(&mut eq, peak(300.0, 12.0, 1.0));
        assert!(did.starts_with("removed") && !eq.iter().any(|b| b.kind == EqKind::Peak && b.hz == 300.0), "{did}");
        let mut eq = vec![];
        merge(&mut eq, peak(30000.0, 20.0, 50.0));
        assert_eq!(eq, [peak(20000.0, 12.0, 10.0)]);
        let mut master = vec![peak(4000.0, 4.5, 0.7)];
        assert!(merge_eq(&mut master, peak(4500.0, 3.0, 0.7), MASTER_EQ_MAX_DB).is_ok());
        assert_eq!(master, [peak(4000.0, 6.0, 0.7)], "master bands stop at 6 dB");
    }

    #[test]
    fn tracks_resolve_by_exact_name_then_one_case_insensitive_match() {
        let p = project(json!([
            {"op": "add_track", "name": "Pad", "instrument": {"kind": "none"}},
            {"op": "add_track", "name": "lead", "instrument": {"kind": "none"}},
            {"op": "add_track", "name": "LEAD", "instrument": {"kind": "none"}},
        ]));
        assert_eq!(resolve_track(&p, "Pad"), Ok(0));
        assert_eq!(resolve_track(&p, "pad"), Ok(0));
        assert_eq!(resolve_track(&p, "lead"), Ok(1));
        assert_eq!(resolve_track(&p, "LEAD"), Ok(2));
        assert!(resolve_track(&p, "Lead").unwrap_err().contains("2 tracks"));
        assert!(resolve_track(&p, "Drums").unwrap_err().contains("no track"));
    }

    #[test]
    fn mix_changes_map_onto_the_mixer() {
        let p = routed();
        let pad = resolve_track(&p, "Pad").unwrap();
        let mut m = p.tracks[pad].mixer.clone();
        let ok = |m: &mut Mixer, c: MixChange| mix_change(&p, pad, m, &c).unwrap_or_else(|e| panic!("{c:?}: {e}"));
        ok(&mut m, MixChange::EqCut { hz: 300.0, db: -3.0, q: 1.4 });
        ok(&mut m, MixChange::EqBoost { hz: 320.0, db: 1.0, q: 1.0 });
        assert_eq!(m.eq, [peak(300.0, -2.0, 1.4)]);
        ok(&mut m, MixChange::HighPass { hz: 119.6 });
        assert_eq!(m.hpf_hz, 120.0);
        ok(&mut m, MixChange::Gain { db: -2.0 });
        assert_eq!(m.gain_db, -2.0);
        ok(&mut m, MixChange::ReverbSend { scale: 0.5 });
        assert_eq!((m.reverb_send, m.sends[0].level, m.sends[1].level), (0.1, 0.2, 0.5), "the send to the reverb return scales, not the delay's");
        ok(&mut m, MixChange::DelayMix { scale: 0.5 });
        assert_eq!((m.delay.unwrap().mix, m.sends[0].level, m.sends[1].level), (0.2, 0.2, 0.25));
        ok(&mut m, MixChange::MonoBelow { hz: 150.0 });
        assert_eq!(m.mono_below_hz, 150.0);
        ok(&mut m, MixChange::DeEss { hz: 6500.0, range_db: -4.0 });
        assert_eq!(m.deess, Some(DeEss { hz: 6500.0, range_db: -4.0, ..DeEss::default() }));
        ok(&mut m, MixChange::DeEss { hz: 6500.0, range_db: -4.0 });
        assert_eq!(m.deess.unwrap().threshold_db, DeEss::default().threshold_db - 3.0, "the same de-essing again catches more");
        ok(&mut m, MixChange::DeEss { hz: 5000.0, range_db: -4.0 });
        assert_eq!(m.deess.unwrap().hz, 5000.0);

        let no = |m: &mut Mixer, ti: usize, c: MixChange| mix_change(&p, ti, m, &c).unwrap_err();
        let before = m.clone();
        assert!(no(&mut m, pad, MixChange::HighPass { hz: 80.0 }).contains("already"));
        assert!(no(&mut m, pad, MixChange::MonoBelow { hz: 120.0 }).contains("already"));
        m.gain_db = 24.0;
        assert!(no(&mut m, pad, MixChange::Gain { db: 3.0 }).contains("limit"));
        m.gain_db = before.gain_db;
        m.deess.as_mut().unwrap().threshold_db = -30.0;
        assert!(no(&mut m, pad, MixChange::DeEss { hz: 5000.0, range_db: -4.0 }).contains("deepest"));
        m.deess = before.deess;
        assert_eq!(m, before, "a fix that can't be carried out changes nothing");

        let dry = resolve_track(&p, "Dry").unwrap();
        let mut d = p.tracks[dry].mixer.clone();
        assert!(no(&mut d, dry, MixChange::ReverbSend { scale: 0.5 }).contains("no reverb send"));
        assert!(no(&mut d, dry, MixChange::DelayMix { scale: 0.5 }).contains("no delay"));
        // a reverb return fed by others keeps its own mix: the senders' sends are the ones to turn down
        let verb = resolve_track(&p, "Verb").unwrap();
        let mut v = p.tracks[verb].mixer.clone();
        assert!(no(&mut v, verb, MixChange::ReverbSend { scale: 0.5 }).contains("no reverb send"));
    }

    #[test]
    fn chosen_fixes_make_one_set_mixer_per_track() {
        let p = routed();
        let fixes = [
            fix("Pad", MixChange::EqCut { hz: 300.0, db: -3.0, q: 1.4 }),
            fix("Pad", MixChange::HighPass { hz: 100.0 }),
            fix("Ghost", MixChange::Gain { db: -1.0 }),
            fix("Pad", MixChange::HighPass { hz: 80.0 }),
            fix("dry", MixChange::Gain { db: -1.5 }),
            fix("Pad", MixChange::Gain { db: 1.0 }),
        ];
        let (cmds, applied, skipped) = plan_mix(&p, &fixes, &[0, 1, 2, 3, 4, 7]);
        assert_eq!(indices(&applied), [0, 1, 4]);
        assert_eq!(indices(&skipped), [2, 3, 7]);
        assert!(skipped[0]["why"].as_str().unwrap().contains("no track"));
        assert!(skipped[1]["why"].as_str().unwrap().contains("already at 100 Hz"), "{}", skipped[1]);
        assert_eq!(applied[2]["track"], "Dry");
        assert_eq!(cmds.len(), 2);
        let Command::SetMixer { track: TrackRef::Id(id), mixer } = &cmds[0] else { panic!("{:?}", cmds[0]) };
        assert_eq!(*id, p.tracks[0].id);
        assert_eq!((mixer.eq.as_deref(), mixer.hpf_hz), (Some(&[peak(300.0, -3.0, 1.4)][..]), Some(100.0)));
        assert!(mixer.gain_db.is_none() && mixer.sends.is_none() && mixer.reverb.is_none() && mixer.delay.is_none());
        let mut after = p.clone();
        for c in &cmds {
            flux_core::commands::apply(&mut after, c).unwrap();
        }
        assert_eq!(after.tracks[1].mixer.gain_db, -1.5);
        assert_eq!(after.tracks[2].mixer, p.tracks[2].mixer);
        assert!(mixer_patch(&p.tracks[0].mixer, &p.tracks[0].mixer).is_none());
    }

    #[test]
    fn split_moves_find_the_band_by_frequency() {
        let x = Multiband::default().crossovers_hz;
        assert_eq!(x, [150.0, 2500.0, 8000.0]);
        for ((lo, hi), band) in [((200.0, 500.0), 1), ((3000.0, 6000.0), 2), ((40.0, 100.0), 0), ((10000.0, 16000.0), 3), ((100.0, 300.0), 1), ((6000.0, 9000.0), 2)] {
            assert_eq!(split_band(&x, lo, hi), band, "{lo}-{hi} Hz");
        }
        for ((lo, hi), band) in [((200.0, 500.0), 1), ((5000.0, 8000.0), 2), ((50.0, 100.0), 0), ((1000.0, 1000.0), 1)] {
            assert_eq!(split_band(&[300.0, 3000.0], lo, hi), band, "{lo}-{hi} Hz");
        }
        // the report's stereo bands land on the imager's bands one to one
        for (i, w) in split_edges(&STEREO_CROSSOVERS_HZ).iter().enumerate() {
            assert_eq!(split_band(&STEREO_CROSSOVERS_HZ, w.0, w.1), i);
        }
    }

    #[test]
    fn the_master_stages_turn_on_and_deepen_with_their_moves() {
        let base = Master::default();
        let mv = |m: &mut Master, x: MasterMove| master_move(m, &x, &mut BTreeSet::new(), -15.0, false);
        let ok = |m: &mut Master, x: MasterMove| mv(m, x.clone()).unwrap_or_else(|e| panic!("{x:?}: {e}"));

        // a side-only EQ band is a band of its own beside a stereo one at the same frequency
        let mut m = base.clone();
        ok(&mut m, MasterMove::Eq { shape: EqShape::Peak, hz: 3000.0, db: -1.5, q: 1.0, part: Part::Stereo });
        let did = ok(&mut m, MasterMove::Eq { shape: EqShape::HighShelf, hz: 8000.0, db: -2.0, q: 0.7, part: Part::Side });
        ok(&mut m, MasterMove::Eq { shape: EqShape::Peak, hz: 3000.0, db: -1.0, q: 1.0, part: Part::Side });
        assert!(did.contains("side high shelf"), "{did}");
        assert_eq!(m.eq.iter().map(|b| (b.hz, b.db, b.channel)).collect::<Vec<_>>(), [
            (3000.0, -1.5, Channel::Stereo),
            (8000.0, -2.0, Channel::Side),
            (3000.0, -1.0, Channel::Side)
        ]);
        assert!(valid(&base, &m));

        // a high-pass only rises and steepens
        let mut m = base.clone();
        ok(&mut m, MasterMove::HighPass { hz: 20.0, slope: 24 });
        let hp: Vec<_> = m.eq.iter().filter(|b| b.kind == EqKind::HighPass).map(|b| (b.hz, b.slope)).collect();
        assert_eq!(hp, [(20.0, 24)]);
        assert!(valid(&base, &m));
        let mut at = m.clone();
        assert!(mv(&mut at, MasterMove::HighPass { hz: 15.0, slope: 12 }).unwrap_err().contains("already at 20 Hz"));
        assert_eq!(at, m);

        // the de-esser turns on, then reaches lower and deeper, then catches more, down to its deepest
        let mut m = base.clone();
        ok(&mut m, MasterMove::DeEss { hz: 7000.0, range_db: -4.0 });
        assert_eq!(m.deess, Some(DeEss { hz: 7000.0, range_db: -4.0, threshold_db: DEESS_THRESHOLD_DB }));
        ok(&mut m, MasterMove::DeEss { hz: 6000.0, range_db: -6.0 });
        ok(&mut m, MasterMove::DeEss { hz: 6000.0, range_db: -6.0 });
        assert_eq!(m.deess, Some(DeEss { hz: 6000.0, range_db: -6.0, threshold_db: DEESS_THRESHOLD_DB - 3.0 }));
        assert!(valid(&base, &m));
        m.deess.as_mut().unwrap().threshold_db = -30.0;
        assert!(mv(&mut m, MasterMove::DeEss { hz: 6000.0, range_db: -6.0 }).unwrap_err().contains("deepest"));

        // a dynamic EQ band, deepened by the next move on the same range until it's as deep as a move takes it
        let mut m = base.clone();
        let harsh = MasterMove::DynEq { shape: EqShape::Peak, hz: 3160.0, q: 0.9, range_db: -4.0 };
        ok(&mut m, harsh.clone());
        assert_eq!((m.dyn_eq.len(), m.dyn_eq[0].hz, m.dyn_eq[0].range_db, m.dyn_eq[0].threshold_db), (1, 3160.0, -4.0, -4.0));
        ok(&mut m, MasterMove::DynEq { shape: EqShape::Peak, hz: 3500.0, q: 0.9, range_db: -4.0 });
        ok(&mut m, MasterMove::DynEq { shape: EqShape::HighShelf, hz: 10000.0, q: 0.7, range_db: -3.0 });
        assert_eq!(m.dyn_eq.iter().map(|b| (b.kind, b.hz, b.range_db)).collect::<Vec<_>>(), [
            (EqKind::Peak, 3160.0, -8.0),
            (EqKind::HighShelf, 10000.0, -3.0)
        ]);
        assert!(valid(&base, &m));
        for _ in 0..6 {
            let _ = mv(&mut m, harsh.clone());
        }
        assert_eq!((m.dyn_eq[0].range_db, m.dyn_eq[0].threshold_db), (-12.0, -12.0));
        assert!(mv(&mut m, harsh).unwrap_err().contains("deepest"));

        // the exciter turns on with only the band asked for, which then only blends in more
        let mut m = base.clone();
        let air = MasterMove::Exciter { hz_lo: 2500.0, hz_hi: 8000.0, mix: 0.5, mode: Saturation::Tape };
        let did = ok(&mut m, air.clone());
        assert!(did.contains("turned on"), "{did}");
        let e = m.exciter.clone().unwrap();
        assert_eq!(e.bands.iter().map(|b| b.mix).collect::<Vec<_>>(), [0.0, 0.0, 0.5, 0.0]);
        assert!(valid(&base, &m));
        assert!(mv(&mut m, air).unwrap_err().contains("already blends in 50 %"));
        ok(&mut m, MasterMove::Exciter { hz_lo: 9000.0, hz_hi: 16000.0, mix: 0.3, mode: Saturation::Tube });
        assert_eq!((m.exciter.as_ref().unwrap().bands[3].mix, m.exciter.as_ref().unwrap().bands[3].mode), (0.3, SatMode::Tube));

        // the imager splits where the report measures, and its moves scale a band's width
        let mut m = base.clone();
        ok(&mut m, MasterMove::Imager { hz_lo: 5000.0, hz_hi: 20000.0, width: 0.8 });
        assert_eq!(m.imager, Some(Imager { crossovers_hz: STEREO_CROSSOVERS_HZ.to_vec(), widths: vec![1.0, 1.0, 1.0, 0.8] }));
        ok(&mut m, MasterMove::Imager { hz_lo: 150.0, hz_hi: 800.0, width: 1.2 });
        assert_eq!(m.imager.as_ref().unwrap().widths, [1.0, 1.2, 1.0, 0.8]);
        assert!(valid(&base, &m));

        // one change per stage band and pass
        let mut done = BTreeSet::new();
        let mut m = base.clone();
        master_move(&mut m, &MasterMove::DeEss { hz: 7000.0, range_db: -4.0 }, &mut done, -15.0, false).unwrap();
        let again = master_move(&mut m, &MasterMove::DeEss { hz: 6000.0, range_db: -6.0 }, &mut done, -15.0, false);
        assert!(again.unwrap_err().contains("already changed"));
    }

    #[test]
    fn master_moves_merge_claim_and_respect_the_delivery_target() {
        let base = Master::default();
        let mv = |m: MasterMove| Some(m);
        let issues = [
            issue("muddy", mv(MasterMove::Eq { shape: EqShape::Peak, hz: 250.0, db: -2.0, q: 1.0, part: Part::Stereo })),
            issue("boxy", mv(MasterMove::Eq { shape: EqShape::Peak, hz: 270.0, db: -1.5, q: 1.0, part: Part::Stereo })),
            issue("too_wide", mv(MasterMove::Width { width: 0.8 })),
            issue("wide_again", mv(MasterMove::Width { width: 0.9 })),
            issue("loudness_target", mv(MasterMove::Loudness { target_lufs: -17.0 })),
            issue("seam", None),
            issue("true_peak", mv(MasterMove::Limiter { release_ms: None, clip_db: Some(1.5) })),
            issue("mud_when_loud", mv(MasterMove::DynEq { shape: EqShape::Peak, hz: 320.0, q: 0.9, range_db: -4.0 })),
            issue("wide_low_end", mv(MasterMove::MonoBelow { hz: 120.0 })),
            issue("harsh_when_loud", mv(MasterMove::DynEq { shape: EqShape::Peak, hz: 3160.0, q: 0.9, range_db: -3.0 })),
            issue("mud_again", mv(MasterMove::DynEq { shape: EqShape::Peak, hz: 350.0, q: 0.9, range_db: -2.0 })),
            issue("sibilance", mv(MasterMove::DeEss { hz: 7000.0, range_db: -6.0 })),
        ];
        let all: Vec<usize> = (0..issues.len()).collect();
        let (m, applied, skipped) = plan_master(&base, &issues, &all, false, -15.0);
        assert_eq!(indices(&applied), [0, 2, 6, 7, 8, 9, 11]);
        assert_eq!(indices(&skipped), [1, 3, 4, 10], "an issue without a move is left out unless picked");
        assert!(skipped[0]["why"].as_str().unwrap().contains("EQ band at 250 Hz"), "each EQ band once a pass: {}", skipped[0]);
        assert!(skipped[2]["why"].as_str().unwrap().contains("pick this issue"));
        assert_eq!(m.eq, [peak(250.0, -2.0, 1.0)]);
        assert_eq!((m.width, m.mono_below_hz, m.target_lufs, m.limiter.clip_db), (0.8, 120.0, -15.0, 1.5));
        assert!(skipped[3]["why"].as_str().unwrap().contains("dynamic EQ band at 320 Hz"), "{}", skipped[3]);
        assert_eq!(m.dyn_eq.iter().map(|b| (b.hz, b.range_db)).collect::<Vec<_>>(), [(320.0, -4.0), (3160.0, -3.0)]);
        assert_eq!(m.deess, Some(DeEss { hz: 7000.0, range_db: -6.0, threshold_db: DEESS_THRESHOLD_DB }));
        assert_eq!(m.multiband, base.multiband, "moves leave the multiband to presets and the agent");
        assert!(valid(&base, &m));

        // picked by index: the target move is taken, and a move-less issue says why it was not
        let (m, applied, skipped) = plan_master(&base, &issues, &[4, 5], true, -15.0);
        assert_eq!((indices(&applied), indices(&skipped), m.target_lufs), (vec![4], vec![5], -17.0));
        let near = [issue("loudness_target", mv(MasterMove::Loudness { target_lufs: -15.5 })), issue("same", mv(MasterMove::Loudness { target_lufs: -15.0 }))];
        let (m, applied, skipped) = plan_master(&base, &near, &[0, 1], false, -15.0);
        assert_eq!((indices(&applied), indices(&skipped), m.target_lufs), (vec![0], vec![1], -15.5));
        assert!(skipped[0]["why"].as_str().unwrap().contains("already changed"), "one move per setting and pass");
        let (_, _, skipped) = plan_master(&base, &near, &[1], false, -15.0);
        assert!(skipped[0]["why"].as_str().unwrap().contains("already aims"));

        // the next pass folds into the band, up to the master's limit
        let (pass1, ..) = plan_master(&base, &issues[..1], &[0], false, -15.0);
        let (m, applied, _) = plan_master(&pass1, &issues[1..2], &[0], false, -15.0);
        assert_eq!((applied.len(), m.eq.as_slice()), (1, &[peak(250.0, -3.5, 1.0)][..]));
        let mut deep = base.clone();
        deep.eq = vec![peak(250.0, -6.0, 1.0)];
        let (_, _, skipped) = plan_master(&deep, &issues[..1], &[0], false, -15.0);
        assert!(skipped[0]["why"].as_str().unwrap().contains("belongs in the mix"), "{}", skipped[0]);

        let mut narrow = base.clone();
        narrow.width = 0.6;
        let (m, ..) = plan_master(&narrow, &issues[2..3], &[0], false, -15.0);
        assert_eq!(m.width, 0.5);
        narrow.width = 0.5;
        let (_, applied, _) = plan_master(&narrow, &issues[2..3], &[0], false, -15.0);
        assert!(applied.is_empty());
        assert!(master_patch(&base, &base.clone()).is_none());
        let mut wider = base.clone();
        wider.width = 1.2;
        let patch = master_patch(&base, &wider).unwrap();
        assert!(patch.width == Some(1.2) && patch.eq.is_none() && patch.multiband.is_none() && patch.limiter.is_none());
        assert!(patch.dyn_eq.is_none() && patch.deess.is_none() && patch.exciter.is_none() && patch.imager.is_none() && patch.eq_phase.is_none());
    }

    #[test]
    fn presets_set_the_chain_and_never_the_targets() {
        let mut base = Master { target_lufs: -14.0, ceiling_dbtp: -1.5, width: 1.2, ..Master::default() };
        base.eq.push(peak(3000.0, -1.0, 1.0));
        for name in PRESETS {
            let mut m = base.clone();
            preset(name).unwrap().apply_to(&mut m);
            assert_eq!((m.target_lufs, m.ceiling_dbtp, m.width, &m.eq), (-14.0, -1.5, 1.2, &base.eq), "{name}");
            if m != base {
                assert!(valid(&base, &m), "{name} does not validate");
            }
        }
        let mut m = base.clone();
        preset("transparent").unwrap().apply_to(&mut m);
        assert_eq!((m.glue, m.multiband, m.limiter, m.mono_below_hz), (base.glue, None, Limiter::default(), base.mono_below_hz));
        assert!(preset("loud").err().is_some_and(|e| e.to_string().contains("game_loop")));
    }

    // -----------------------------------------------------------------------------------------
    // end to end: the tools on a short song of audio tracks (no instruments needed)

    /// The end-to-end tests share the tools' caches of the last check: one at a time.
    static SONGS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const SR: u32 = 48_000;
    /// 4 bars of 4/4 at 120 bpm.
    const LEN_S: f64 = 8.0;

    /// Sines at `hz` for the whole song, the right channel `phase` radians behind the left (0: mono).
    fn tones(hz: &[f64], level: f64, phase: f64) -> (Vec<f32>, Vec<f32>) {
        let n = (LEN_S * SR as f64) as usize;
        let at = |k: usize, ph: f64| hz.iter().map(|f| level * (std::f64::consts::TAU * f * k as f64 / SR as f64 - ph).sin()).sum::<f64>() as f32;
        ((0..n).map(|k| at(k, 0.0)).collect(), (0..n).map(|k| at(k, phase)).collect())
    }

    /// A decaying burst at `hz` on every beat, `offset` beats late.
    fn hits(hz: f64, offset: f64) -> (Vec<f32>, Vec<f32>) {
        let n = (LEN_S * SR as f64) as usize;
        let mut l = vec![0f32; n];
        let mut beat = offset;
        while beat * 0.5 < LEN_S {
            let s = (beat * 0.5 * SR as f64) as usize;
            for k in 0..(SR as usize / 8).min(n - s) {
                let x = k as f64 / SR as f64;
                l[s + k] += (0.6 * (-x * 30.0).exp() * (std::f64::consts::TAU * hz * x).sin()) as f32;
            }
            beat += 1.0;
        }
        (l.clone(), l)
    }

    /// A looped song: kick and snare hits, a bass wide in the low end, and a pad and keys crowding the low mids.
    fn song(name: &str) -> (Arc<Shared>, PathBuf) {
        let d = std::env::temp_dir().join(format!("flux-measure-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let parts = [
            ("Kick", hits(55.0, 0.0)),
            ("Snare", hits(190.0, 0.5)),
            ("Bass", tones(&[55.0, 110.0], 0.2, std::f64::consts::FRAC_PI_2)),
            ("Pad", tones(&[220.0, 277.0, 330.0], 0.12, 0.3)),
            ("Keys", tones(&[262.0, 330.0, 392.0], 0.12, 0.3)),
        ];
        let mut cmds = vec![json!({"op": "set_project", "bars": 4, "looped": true})];
        for (track, audio) in &parts {
            let f = d.join(format!("{track}.wav"));
            flux_engine::render::write_wav_f32(&f, SR, audio).unwrap();
            cmds.push(json!({"op": "add_track", "name": track, "instrument": {"kind": "audio", "path": f.to_string_lossy()}}));
        }
        cmds.push(json!({"op": "set_mixer", "track": "Snare", "mixer": {"reverb_send": 0.6}}));
        let mut p = Project::new(name, 120.0, 4);
        p.sample_rate = SR;
        for c in serde_json::from_value::<Vec<Command>>(Value::Array(cmds)).unwrap() {
            flux_core::commands::apply(&mut p, &c).unwrap();
        }
        (Shared::new(Session::new(p)), d)
    }

    fn ask(sh: &Arc<Shared>, tool: &str, a: Value) -> Value {
        match call(sh, tool, &a).unwrap_or_else(|e| panic!("{tool} {a}: {e:#}")).first() {
            Some(Content::Text(t)) => serde_json::from_str(t).unwrap(),
            _ => panic!("{tool} replied without text"),
        }
    }

    fn refused(sh: &Arc<Shared>, tool: &str, a: Value) -> String {
        match call(sh, tool, &a) {
            Ok(_) => panic!("{tool} {a} should have been refused"),
            Err(e) => format!("{e:#}"),
        }
    }

    fn log_len(sh: &Arc<Shared>) -> usize {
        sh.lock().session.log.len()
    }

    /// The last activity entry: by the agent, and its summary.
    fn last_entry(sh: &Arc<Shared>) -> String {
        let st = sh.lock();
        let e = st.session.log.last().unwrap();
        assert!(e.ok && e.source == flux_core::Source::Agent);
        e.summary.clone()
    }

    #[test]
    fn mix_check_applies_its_fixes_as_one_undo_step() {
        let _one = SONGS.lock().unwrap_or_else(|e| e.into_inner());
        let (sh, dir) = song("mix-check-test");
        let check = ask(&sh, "mix_check", json!({}));
        let fixes = check["fixes"].as_array().unwrap();
        assert!(!fixes.is_empty(), "{check}");
        assert_eq!(indices(fixes), (0..fixes.len() as u64).collect::<Vec<_>>());
        let regions = check["regions"].as_array().unwrap();
        assert!(!regions.is_empty() && regions.iter().all(|r| r["stems"][0]["stem"].is_string()), "{check}");

        let mixers = |sh: &Arc<Shared>| sh.lock().session.project.tracks.iter().map(|t| t.mixer.clone()).collect::<Vec<_>>();
        let (before, entries) = (mixers(&sh), log_len(&sh));
        let none = ask(&sh, "mix_check", json!({"apply": [99]}));
        assert_eq!((none["applied"].as_array().unwrap().len(), log_len(&sh)), (0, entries), "{none}");
        assert!(none["skipped"][0]["why"].as_str().unwrap().contains("no fix 99"));

        let done = ask(&sh, "mix_check", json!({"apply": true}));
        let applied = done["applied"].as_array().unwrap();
        assert!(!applied.is_empty(), "{done}");
        assert_eq!(applied.len() + done["skipped"].as_array().unwrap().len(), fixes.len());
        assert_eq!(log_len(&sh), entries + 1, "one undo step");
        assert!(last_entry(&sh).starts_with("mix_check: "));
        assert_eq!(done["revision"].as_u64(), Some(sh.lock().session.revision));
        for k in ["conflicts", "buried", "flags", "fixes", "sections"] {
            assert!(!done["before"][k].is_null() && !done["after"][k].is_null(), "{k}: {done}");
        }
        assert_ne!(mixers(&sh), before);
        assert!(sh.with_session(|s| s.undo(flux_core::Source::Agent)));
        assert_eq!(mixers(&sh), before, "one undo reverts every fix");

        // indices only count for the check of the project as it is
        assert!(refused(&sh, "mix_check", json!({"apply": [0]})).contains("changed since the last mix_check"));
        assert!(refused(&sh, "mix_check", json!({"file": "song.wav", "apply": true})).contains("without `file`"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn master_checks_applies_and_sets_presets_one_undo_step_each() {
        let _one = SONGS.lock().unwrap_or_else(|e| e.into_inner());
        let (sh, dir) = song("master-test");
        let original = sh.lock().session.project.master.clone();
        let check = ask(&sh, "master", json!({}));
        assert!(check["score"].as_u64().unwrap() <= 100, "{check}");
        let issues = check["issues"].as_array().unwrap();
        assert!(!issues.is_empty(), "{check}");
        assert_eq!(indices(issues), (0..issues.len() as u64).collect::<Vec<_>>());
        assert!(check["chain"]["limiter_ceiling_dbtp"].is_number() && check["loudness"]["target_lufs"] == -15.0, "{check}");
        assert!(check["tonal"]["closest_style"].is_string(), "{check}");

        let entries = log_len(&sh);
        let none = ask(&sh, "master", json!({"action": "apply", "apply": [999]}));
        assert_eq!((none["note"].as_str(), log_len(&sh)), (Some("nothing was changed"), entries), "{none}");
        let done = ask(&sh, "master", json!({"action": "apply"}));
        assert!(!done["applied"].as_array().unwrap().is_empty(), "{done}");
        assert_eq!(log_len(&sh), entries + 1);
        assert!(last_entry(&sh).starts_with("master apply"));
        assert!(done["before"]["score"].is_u64() && done["after"]["score"] == done["check"]["score"], "{done}");
        assert!(sh.with_session(|s| s.undo(flux_core::Source::Agent)));
        assert_eq!(sh.lock().session.project.master, original);
        assert!(refused(&sh, "master", json!({"action": "apply", "apply": [0]})).contains("indices must come from a check"));

        let set = ask(&sh, "master", json!({"action": "preset", "name": "game_loop"}));
        assert!(set["set"]["multiband"].is_object() && set["set"]["mono_below_hz"] == 120.0, "{set}");
        assert_eq!(last_entry(&sh), "master preset game_loop");
        let m = sh.lock().session.project.master.clone();
        assert_eq!((m.target_lufs, m.ceiling_dbtp, m.eq), (original.target_lufs, original.ceiling_dbtp, original.eq));
        let again = ask(&sh, "master", json!({"action": "preset", "name": "game_loop"}));
        assert!(again["note"].is_string() && again["score_before"] == again["score"], "{again}");

        let entries = log_len(&sh);
        let auto = ask(&sh, "master", json!({"action": "auto", "passes": 2}));
        let passes = auto["passes"].as_array().unwrap();
        assert!(!passes.is_empty() && passes.len() <= 2, "{auto}");
        assert!(auto["score_after"].as_u64() >= auto["score_before"].as_u64(), "a pass that loses points is undone: {auto}");
        let undone = passes.iter().filter(|p| p["undone"] == true).count();
        let changed = passes.iter().filter(|p| p["score_after"].is_u64()).count();
        assert_eq!(log_len(&sh), entries + changed + undone, "one entry per pass, and one per undo");

        assert!(refused(&sh, "master", json!({"action": "match"})).contains("needs a reference"));
        assert!(refused(&sh, "master", json!({"action": "preset", "name": "loud"})).contains("unknown preset"));
        assert!(refused(&sh, "master", json!({"target_lufs": 3})).contains("-40 to 0"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
