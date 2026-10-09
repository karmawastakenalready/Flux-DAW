//! MCP tools that measure with `flux-analysis`: mix problems and the tracks causing them, a reference
//! comparison, the game's music judged as a set, spectrograms and instrument report cards. `analyze` (tools.rs)
//! reads the render as one signal; mix_check, reference_match and spectrogram also take the tracks apart, from a
//! render that keeps every track's stem and the dry master.

use crate::state::{LastRender, Shared};
use crate::tools::{ensure_render, get, rel, req, text, tool, Content};
use anyhow::{anyhow, bail, ensure, Context, Result};
use flux_analysis::card::catalog::{self, Isolation, RunOptions};
use flux_analysis::card::CardOptions;
use flux_analysis::loudness::{check_game_with, GameInput, GameOptions};
use flux_analysis::meta::{LayersManifest, Section};
use flux_analysis::mix::{check_mix, MixOptions};
use flux_analysis::reference::{compare, ReferenceOptions};
use flux_analysis::spectro::SpectroOptions;
use flux_analysis::{Audio, AudioRef};
use flux_core::pitch::PitchSpec;
use flux_core::{Project, TimeMap};
use flux_engine::{paths, RenderOptions, RenderOutput};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
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
is unchanged, up to 10 minutes) or a WAV/FLAC `file`.", json!({
            "file": file_s("a WAV/FLAC mix to check instead of the project"),
            "stems": file_s("with file: a .layers.json manifest or a folder of WAV/FLAC stems that sum to it (names the culprits)"),
            "dry": file_s("with file: the same mix without reverb and delay (enables the smear check)"),
            "sections": sections_s("judge these time ranges separately (default: the project's sections, or the manifest's)"),
            "max_conflicts": {"type": "integer", "minimum": 1, "description": "most masking collisions listed (default 12)"}}), &[]),
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

pub fn separated(sh: &Arc<Shared>) -> Result<Arc<Separated>> {
    let (project, revision) = {
        let st = sh.lock();
        (st.session.project.clone(), st.session.revision)
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(&project)?.hash(&mut h);
    let key = h.finish();
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

fn mix_check(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let sections = get::<Vec<Section>>(a, "sections")?;
    let max_conflicts = get::<usize>(a, "max_conflicts")?.unwrap_or(12).max(1);
    let (source, report) = match get::<String>(a, "file")? {
        Some(f) => {
            let (path, mix) = read_audio(&f)?;
            let (stems, manifest_sections) = load_stems(get::<String>(a, "stems")?.as_deref())?;
            let dry = get::<String>(a, "dry")?.map(|d| read_audio(&d)).transpose()?;
            let views: Vec<(&str, AudioRef)> = stems.iter().map(|(n, x)| (n.as_str(), x.view())).collect();
            let o = MixOptions { sections: sections.unwrap_or(manifest_sections), max_conflicts, ..Default::default() };
            (rel(&path), check_mix(mix.view(), &views, dry.as_ref().map(|d| d.1.view()), &o)?)
        }
        None => {
            let s = separated(sh)?;
            let o = MixOptions { sections: sections.unwrap_or_else(|| Section::of_render(&s.out)), max_conflicts, ..Default::default() };
            let dry = s.out.dry.as_ref().map(|d| view(s.out.sr, d));
            (format!("project revision {}", s.revision), check_mix(AudioRef::master(&s.out), &track_stems(&s.out), dry, &o)?)
        }
    };
    let mut v = serde_json::to_value(&report)?;
    v["source"] = json!(source);
    reply(v)
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
            let dir = paths::root().join("output");
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
