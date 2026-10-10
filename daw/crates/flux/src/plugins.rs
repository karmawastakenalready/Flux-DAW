//! The `plugins` tool: VST3 plugins. Finding installed plugins; a slot's plugin (a track's instrument or an
//! insert effect): its parameters, factory programs and the presets saved in its own format; changing them with
//! validation; capturing the plugin's state into the project; and the plugin's own window for the human (whose
//! edits there become undoable project edits, source=human).

use crate::state::Shared;
use crate::tools::{get, rel, req, text, Content};
use anyhow::{anyhow, bail, Context, Result};
use flux_core::commands::{self, Command};
use flux_core::{Instrument, Project, Source, Track, TrackRef};
use flux_engine::paths;
use flux_engine::vst::{self, EditorNotice, SlotId};
use flux_vst3::client::{Loaded, PluginHost};
use flux_vst3::preset::Preset;
use flux_vst3::scan::{self, ScannedPlugin, Scanner};
use flux_vst3::vendor::{self, Family, VendorPreset};
use flux_vst3::State;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A person's edits are saved this long after they stop (so a knob turn becomes one undo step).
const SETTLE: Duration = Duration::from_millis(1200);

/// How long a sample player (Decent Sampler) may take to load a preset's samples in the background.
const SAMPLE_LOAD: Duration = Duration::from_secs(120);

pub fn tool() -> Value {
    let target = json!({
        "track": crate::tools::track_ref(),
        "insert": {"type": "integer", "description": "an insert effect's id (get_project lists them), instead of track"},
        "path": {"type": "string", "description": "a .vst3 plugin not in the project (instead of track/insert)"},
        "class": {"type": "string", "description": "with path: which plugin in a bundle that holds several"},
    });
    let mut props: Map<String, Value> = target.as_object().cloned().unwrap_or_default();
    for (k, v) in json!({
        "action": {"enum": ["scan", "info", "params", "set_params", "programs", "set_program", "presets", "load_preset", "capture", "open_editor", "close_editor"]},
        "query": {"type": "string", "description": "scan: substring of name/vendor/category; params: of the parameter title; programs/presets: of the name (presets: or its pack/folder)"},
        "kind": {"enum": ["instrument", "effect", "any"], "description": "scan: default any"},
        "rescan": {"type": "boolean", "description": "scan: ignore the cache and look at every plugin again"},
        "offset": {"type": "integer", "minimum": 0}, "limit": {"type": "integer", "minimum": 1},
        "include_hidden": {"type": "boolean", "description": "params: also parameters the plugin marks hidden"},
        "params": {"type": "object", "description": "set_params: name -> value, as in a vst3 instrument's params"},
        "program": {"description": "set_program: program name or 0-based index. load_preset: the program inside a bank file given as preset (STL Tones)"},
        "list": {"description": "set_program: program list id or name (default: the first)"},
        "preset": {"description": "load_preset: a preset's name or pack/name (as presets lists it), its index there, or a preset file"},
        "keep_params": {"type": "boolean", "description": "set_program/load_preset/capture: keep the slot's params (applied on top); default false"},
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
    {
        props.insert(k, v);
    }
    json!({
        "name": "plugins",
        "description": "VST3 plugins. scan: installed plugins (cached; instrument/effect, vendor, categories). For a slot's plugin (a track's \
instrument, or an insert effect by id) or any plugin by path: info (buses, latency, parameter/program counts, preset format, whether it has \
a window); params (current values as the plugin displays them, defaults, choices of stepped parameters; filter with query, page with \
offset/limit); set_params (checks names and values against the plugin, reports what each resolved to, then saves them to the slot: one \
undo step); programs (the plugin's factory programs); set_program (loads one and saves the resulting state as the slot's preset); \
presets (presets saved in the plugin's own format: Vital, Valhalla, Decent Sampler and STL Tones files in their standard folders); \
load_preset (loads one of those, headless, and saves the resulting state as the slot's preset). capture saves the plugin's current \
state as the slot's preset. open_editor shows the plugin's own window to the human; what they change there is saved to the project \
automatically (source=human, visible in activity) a moment after they stop and when they close it.",
        "inputSchema": {"type": "object", "properties": props, "required": ["action"]},
    })
}

pub fn call(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    match req::<String>(a, "action")?.as_str() {
        "scan" => scan(a),
        "info" => info(sh, a),
        "params" => params(sh, a),
        "set_params" => set_params(sh, a),
        "programs" => programs(sh, a),
        "set_program" => set_program(sh, a),
        "presets" => presets(sh, a),
        "load_preset" => load_preset(sh, a),
        "capture" => capture(sh, a),
        "open_editor" => open_editor(sh, a),
        "close_editor" => close_editor(sh, a),
        x => bail!("unknown action '{x}'"),
    }
}

// ---- scanning ---------------------------------------------------------------------------------------

/// Folders searched for plugins: the system's VST3 folders and the music project's own.
pub fn plugin_dirs() -> Vec<PathBuf> {
    let root = paths::root();
    let mut dirs = scan::standard_dirs();
    for d in ["plugins", "instruments/vst", "daw/plugins"] {
        let p = root.join(d);
        if p.is_dir() {
            dirs.push(p);
        }
    }
    dirs
}

fn cache_file() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Flux").join("plugin-scan.json"))
}

pub fn scanner(rescan: bool) -> Scanner {
    let c = vst::host_command();
    Scanner { exe: c.exe, args: c.args, cache: cache_file(), rescan, ..Scanner::default() }
}

fn scanned_json(p: &ScannedPlugin) -> Value {
    let procs: Vec<_> = p.processors().collect();
    let first = procs.first();
    let mut j = json!({
        "name": p.name(),
        "vendor": p.vendor(),
        "kind": if p.is_instrument() { "instrument" } else { "effect" },
        "categories": first.map(|c| c.sub_categories.clone()).unwrap_or_default(),
        "version": first.map(|c| c.version.clone()).unwrap_or_default(),
        "path": rel(&p.path),
    });
    if procs.len() > 1 {
        j["classes"] = json!(procs.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    }
    if let Some(f) = first.and_then(|c| vendor::family(&c.name, &c.vendor)) {
        j["preset_format"] = json!(f);
    }
    if let Some(e) = &p.error {
        j["error"] = json!(e);
    }
    j
}

fn scan(a: &Value) -> Result<Vec<Content>> {
    let q = get::<String>(a, "query")?.unwrap_or_default().to_lowercase();
    let kind = get::<String>(a, "kind")?.unwrap_or_else(|| "any".into());
    let limit = get::<usize>(a, "limit")?.unwrap_or(100);
    let t0 = Instant::now();
    let found = scanner(get::<bool>(a, "rescan")?.unwrap_or(false)).scan(&scan::find_bundles(&plugin_dirs()));
    let failed = found.iter().filter(|p| !p.ok()).count();
    let list: Vec<Value> = found
        .iter()
        .filter(|p| match kind.as_str() {
            "instrument" => p.is_instrument(),
            "effect" => p.ok() && !p.is_instrument(),
            _ => true,
        })
        .map(scanned_json)
        .filter(|j| q.is_empty() || j.to_string().to_lowercase().contains(&q))
        .collect();
    text(json!({
        "total": list.len(), "shown": list.len().min(limit), "failed_to_scan": failed,
        "plugins": list.into_iter().take(limit).collect::<Vec<_>>(),
        "folders": plugin_dirs().iter().map(|d| rel(d)).collect::<Vec<_>>(),
        "not_installed": crate::supported::missing(&crate::supported::packs(), flux_assets::packs::Section::Plugins, &q),
        "seconds": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        "use": "an instrument plays on a track (add_track/update_track instrument {kind: vst3, path}); an effect goes in an insert slot \
(edit add_insert on a track, the master or the reverb return). not_installed: plugins Flux supports that aren't installed here, and how \
the user gets them",
    }))
}

// ---- a slot's plugin, or one by path --------------------------------------------------------------------

/// The track `r` names, which must play a VST3 plugin.
fn vst_track<'a>(p: &'a Project, r: &TrackRef) -> Result<&'a Track> {
    let i = p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?;
    let t = &p.tracks[i];
    match &t.instrument {
        Instrument::Vst3 { path, .. } if path.ends_with(".rig.json") => bail!("track '{}' plays a rig (.rig.json), not a VST3 plugin", t.name),
        Instrument::Vst3 { .. } => Ok(t),
        _ => bail!("track '{}' doesn't play a VST3 plugin", t.name),
    }
}

/// The project and the slot an action names (a track's VST3 instrument, or an insert), if it names one.
fn slot_of(sh: &Arc<Shared>, a: &Value) -> Result<Option<(Project, SlotId)>> {
    let p = sh.lock().session.project.clone();
    let track = get::<TrackRef>(a, "track")?;
    if let Some(id) = get::<u64>(a, "insert")? {
        if track.is_some() {
            bail!("give a track or an insert, not both");
        }
        p.insert(id).ok_or_else(|| anyhow!("no insert #{id}"))?;
        return Ok(Some((p, SlotId::Insert(id))));
    }
    match track {
        Some(r) => {
            let id = vst_track(&p, &r)?.id;
            Ok(Some((p, SlotId::Instrument(id))))
        }
        None => Ok(None),
    }
}

fn need_slot(sh: &Arc<Shared>, a: &Value, action: &str) -> Result<(Project, SlotId)> {
    slot_of(sh, a)?.ok_or_else(|| anyhow!("{action} works on a track (its instrument) or an insert"))
}

/// The slot, for people: "track 'Pad'", "insert ValhallaSupermassive on the master".
fn label(p: &Project, slot: SlotId) -> String {
    match slot {
        SlotId::Instrument(id) => p.tracks.iter().find(|t| t.id == id).map_or_else(|| format!("track #{id}"), |t| format!("track '{}'", t.name)),
        SlotId::Insert(id) => match p.insert(id) {
            Some((chain, _, ins)) => format!("insert {} on {}", ins.plugin_name(), vst::chain_label(p, chain)),
            None => format!("insert #{id}"),
        },
    }
}

/// The preset file a slot's plugin is set up from, if any.
fn slot_preset(p: &Project, slot: SlotId) -> Option<String> {
    match slot {
        SlotId::Instrument(id) => match &p.tracks.iter().find(|t| t.id == id)?.instrument {
            Instrument::Vst3 { preset, .. } => preset.clone(),
            _ => None,
        },
        SlotId::Insert(id) => p.insert(id)?.2.preset.clone(),
    }
}

/// The slot's params.
fn slot_params(p: &Project, slot: SlotId) -> BTreeMap<String, Value> {
    match slot {
        SlotId::Instrument(id) => match p.tracks.iter().find(|t| t.id == id).map(|t| &t.instrument) {
            Some(Instrument::Vst3 { params, .. }) => params.clone(),
            _ => BTreeMap::new(),
        },
        SlotId::Insert(id) => p.insert(id).map(|(_, _, i)| i.params.clone()).unwrap_or_default(),
    }
}

/// Run `f` on the plugin an action names: a slot's (loaded and set up from the project, and kept loaded) or a
/// plugin by path (loaded just for this call).
fn with_plugin<T>(sh: &Arc<Shared>, a: &Value, f: impl FnOnce(&PluginHost, &Loaded) -> Result<T>) -> Result<T> {
    if let Some((p, slot)) = slot_of(sh, a)? {
        return vst::with_slot(&p, slot, |inst| f(&inst.host, &inst.loaded));
    }
    let path = get::<String>(a, "path")?.ok_or_else(|| anyhow!("give a track, an insert or a plugin path"))?;
    let sr = sh.lock().session.project.sample_rate;
    let (host, loaded, _port) = vst::open_plugin(&paths::resolve(&path), get::<String>(a, "class")?.as_deref(), sr, true)?;
    f(&host, &loaded)
}

fn info(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let l = with_plugin(sh, a, |h, _| h.info())?;
    let i = &l.info;
    let bus = |b: &flux_vst3::plugin::Bus| json!({"name": b.name, "channels": b.channels, "main": b.main});
    let mut j = json!({
        "name": i.class.name, "vendor": i.class.vendor, "version": i.class.version, "categories": i.class.sub_categories,
        "instrument": i.class.is_instrument(), "audio_inputs": i.inputs.iter().map(bus).collect::<Vec<_>>(),
        "audio_outputs": i.outputs.iter().map(bus).collect::<Vec<_>>(), "midi_inputs": i.event_inputs,
        "latency_samples": i.latency, "parameters": i.params, "midi_controllers_mapped": l.midi_mapped,
        "programs": l.program_lists.iter().map(|p| p.programs).sum::<usize>(), "window_open": l.editor_open,
        "sample_rate": i.setup.sample_rate,
    });
    if let Some(f) = vendor::family(&i.class.name, &i.class.vendor) {
        j["preset_format"] = json!(f);
    }
    if let Some((p, slot)) = slot_of(sh, a)? {
        j["slot"] = json!(label(&p, slot));
    }
    text(j)
}

fn params(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let q = get::<String>(a, "query")?;
    let offset = get::<usize>(a, "offset")?.unwrap_or(0);
    let limit = get::<usize>(a, "limit")?.unwrap_or(60);
    let hidden = get::<bool>(a, "include_hidden")?.unwrap_or(false);
    let all = with_plugin(sh, a, |h, _| h.params_with_choices(q.as_deref()))?;
    let shown: Vec<_> = all.iter().filter(|p| hidden || !p.flags.iter().any(|f| f == "hidden")).collect();
    let list: Vec<Value> = shown
        .iter()
        .skip(offset)
        .take(limit)
        .map(|p| {
            let mut j = json!({"title": p.title, "value": p.display, "id": p.id});
            if !p.units.is_empty() {
                j["units"] = json!(p.units);
            }
            if (p.value - p.default).abs() > 1e-6 {
                j["default_normalized"] = json!((p.default * 1000.0).round() / 1000.0);
            }
            j["normalized"] = json!((p.value * 1000.0).round() / 1000.0);
            if !p.choices.is_empty() && p.choices.len() <= 64 {
                j["choices"] = json!(p.choices);
            } else if p.steps > 0 {
                j["steps"] = json!(p.steps);
            }
            let flags: Vec<&String> = p.flags.iter().filter(|f| *f != "automatable").collect();
            if !flags.is_empty() {
                j["flags"] = json!(flags);
            }
            j
        })
        .collect();
    text(json!({
        "total": shown.len(), "offset": offset, "shown": list.len(), "params": list,
        "hint": "set values with set_params using the title and a value in the shown units, a choice's text, or {\"normalized\": x}",
    }))
}

// ---- saving to the project --------------------------------------------------------------------------

/// A change to a slot's plugin set-up.
#[derive(Default)]
struct Change {
    /// A new preset (the plugin's whole state), stored in the project.
    preset: Option<String>,
    /// What the preset is called (None keeps the slot's name).
    preset_name: Option<String>,
    /// Params set (after `clear_params`).
    params: BTreeMap<String, Value>,
    clear_params: bool,
}

/// The edit making `c` to a slot.
fn command(p: &Project, slot: SlotId, c: Change) -> Result<Command> {
    match slot {
        SlotId::Instrument(id) => {
            let t = p.tracks.iter().find(|t| t.id == id).ok_or_else(|| anyhow!("no track #{id}"))?;
            let Instrument::Vst3 { path, params, preset, preset_name, class } = &t.instrument else {
                bail!("track '{}' doesn't play a VST3 plugin", t.name)
            };
            let mut params = if c.clear_params { BTreeMap::new() } else { params.clone() };
            params.extend(c.params);
            let instrument = Instrument::Vst3 {
                path: path.clone(),
                params,
                preset: c.preset.or_else(|| preset.clone()),
                preset_name: c.preset_name.unwrap_or_else(|| preset_name.clone()),
                class: class.clone(),
            };
            Ok(Command::UpdateTrack { track: TrackRef::Id(id), name: None, instrument: Some(instrument), color: None, layer: None })
        }
        SlotId::Insert(id) => Ok(Command::UpdateInsert {
            insert: id,
            params: (!c.params.is_empty()).then_some(c.params),
            clear_params: c.clear_params,
            preset: c.preset.map(Some),
            preset_name: c.preset_name,
            bypass: None,
            position: None,
        }),
    }
}

/// Save a change to a slot's plugin (one undo step). The plugin instance the change was made on already matches
/// it (`from`, the instance a state was captured from, else the render instance) and is marked so first: live
/// playback sets its instance up as soon as it sees the change, and must leave that one be.
fn save(sh: &Arc<Shared>, slot: SlotId, c: Change, source: Source, from: Option<&vst::Captured>) -> Result<()> {
    let p = sh.lock().session.project.clone();
    let cmd = command(&p, slot, c)?;
    let mut after = p;
    commands::apply(&mut after, &cmd)?;
    match from {
        Some(c) => c.mark_applied(&after, slot)?,
        None => vst::note_applied(&after, slot)?,
    }
    sh.apply(&[cmd], source)?;
    Ok(())
}

/// After loading a whole state into a slot's plugin: with `keep`, its params go back on top (as the next render
/// will apply them, so the instance matches the saved slot).
fn reapply_params(host: &PluginHost, p: &Project, slot: SlotId, keep: bool) -> Result<()> {
    let params = slot_params(p, slot);
    if keep && !params.is_empty() {
        host.set_params(&params.into_iter().collect::<Vec<_>>())?;
    }
    Ok(())
}

fn set_params(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (p, slot) = need_slot(sh, a, "set_params")?;
    let values: BTreeMap<String, Value> = req(a, "params")?;
    if values.is_empty() {
        bail!("no params");
    }
    let list: Vec<(String, Value)> = values.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    // checked against the plugin first: nothing is saved unless every name and value resolves
    let set = vst::with_slot(&p, slot, |inst| inst.host.set_params(&list))?;
    save(sh, slot, Change { params: values, ..Change::default() }, Source::Agent, None)?;
    let resolved: Vec<Value> = set.iter().map(|s| json!({"name": s.name, "parameter": s.title, "value": s.display})).collect();
    text(json!({"ok": true, "slot": label(&p, slot), "set": resolved, "revision": sh.lock().session.revision}))
}

// ---- factory programs ---------------------------------------------------------------------------------

fn programs(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let q = get::<String>(a, "query")?.unwrap_or_default().to_lowercase();
    let limit = get::<usize>(a, "limit")?.unwrap_or(200);
    let lists = with_plugin(sh, a, |h, _| h.programs())?;
    let out: Vec<Value> = lists
        .iter()
        // some plugins list a single unnamed placeholder
        .filter(|l| l.programs.iter().any(|n| !n.trim().is_empty()))
        .map(|l| {
            let names: Vec<Value> = l
                .programs
                .iter()
                .enumerate()
                .filter(|(_, n)| q.is_empty() || norm(n).contains(&norm(&q)))
                .take(limit)
                .map(|(i, n)| json!({"index": i, "name": n.trim()}))
                .collect();
            json!({"list": l.id, "name": l.name, "unit": l.unit_name, "count": l.programs.len(), "programs": names})
        })
        .collect();
    let note = if out.is_empty() {
        "this plugin exposes no programs to hosts; try presets (its own preset files), choose sounds in its window (open_editor) and capture, or set params"
    } else {
        "load one with set_program (by name or index)"
    };
    text(json!({"lists": out, "note": note}))
}

/// Lowercase with runs of spaces collapsed (program names are often padded: "LAURIE    ").
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn set_program(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (p, slot) = need_slot(sh, a, "set_program")?;
    let want: Value = req(a, "program")?;
    let list_sel: Option<Value> = get(a, "list")?;
    let keep = get::<bool>(a, "keep_params")?.unwrap_or(false);
    let (name, state) = vst::with_slot(&p, slot, |inst| {
        let lists = inst.host.programs()?;
        let list = match &list_sel {
            None => lists.first(),
            Some(Value::Number(n)) => lists.iter().find(|l| Some(l.id as i64) == n.as_i64()),
            Some(Value::String(s)) => lists.iter().find(|l| l.name.eq_ignore_ascii_case(s)),
            Some(_) => bail!("list must be an id or a name"),
        }
        .ok_or_else(|| anyhow!("no such program list (this plugin has {})", lists.len()))?;
        let index = match &want {
            Value::Number(n) => n.as_u64().map(|i| i as usize).filter(|i| *i < list.programs.len()),
            Value::String(s) => {
                let s = norm(s);
                list.programs.iter().position(|n| norm(n) == s).or_else(|| list.programs.iter().position(|n| norm(n).contains(&s)))
            }
            _ => bail!("program must be a name or an index"),
        }
        .ok_or_else(|| anyhow!("no program {want} in '{}' ({} programs)", list.name, list.programs.len()))?;
        let name = inst.host.set_program(list.id, index)?;
        let state = inst.host.state()?;
        reapply_params(&inst.host, &p, slot, keep)?;
        Ok((name.trim().to_string(), state))
    })?;
    let stored = vst::store_state(&state)?;
    let c = Change { preset: Some(stored.clone()), preset_name: Some(name.clone()), clear_params: !keep, ..Change::default() };
    save(sh, slot, c, Source::Agent, None)?;
    text(json!({"ok": true, "slot": label(&p, slot), "program": name, "preset": stored, "params_kept": keep, "revision": sh.lock().session.revision}))
}

// ---- presets in the plugin's own format ---------------------------------------------------------------

/// The plugin's name and its preset format (None: Flux can't read its own preset files).
fn preset_format(l: &Loaded) -> (String, Option<Family>) {
    let c = &l.info.class;
    (c.name.clone(), vendor::family(&c.name, &c.vendor))
}

fn no_format(name: &str) -> anyhow::Error {
    anyhow!("Flux can't read {name}'s own preset files; use programs/set_program, or open_editor and capture")
}

fn preset_json(i: usize, p: &VendorPreset) -> Value {
    let mut j = json!({"index": i, "name": p.name, "group": p.group, "file": rel(&p.path)});
    if let Some(n) = p.program {
        j["program"] = json!(n);
    }
    j
}

fn presets(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let q = norm(&get::<String>(a, "query")?.unwrap_or_default());
    let offset = get::<usize>(a, "offset")?.unwrap_or(0);
    let limit = get::<usize>(a, "limit")?.unwrap_or(100);
    let (name, family) = with_plugin(sh, a, |_, l| Ok(preset_format(l)))?;
    let family = family.ok_or_else(|| no_format(&name))?;
    let all = vendor::list(family, &name);
    let matching: Vec<Value> = all
        .iter()
        .enumerate()
        .filter(|(_, p)| q.is_empty() || norm(&format!("{}/{}", p.group, p.name)).contains(&q))
        .map(|(i, p)| preset_json(i, p))
        .collect();
    let mut groups: Vec<&str> = all.iter().map(|p| p.group.split('/').next().unwrap_or_default()).collect();
    groups.dedup();
    groups.sort_unstable();
    groups.dedup();
    let note = if all.is_empty() {
        format!("no {} presets found in its folders; put preset files there (or give load_preset a file)", family.extension())
    } else {
        "load one with load_preset (name, pack/name or index); the index is stable while the folders don't change".into()
    };
    text(json!({
        "plugin": name, "format": family, "total": all.len(), "matching": matching.len(), "offset": offset,
        "presets": matching.into_iter().skip(offset).take(limit).collect::<Vec<_>>(),
        "packs": groups, "folders": vendor::dirs(family, &name).iter().map(|d| rel(d)).collect::<Vec<_>>(), "note": note,
    }))
}

/// The preset `want` names: an index into [`vendor::list`], a name or `pack/name` from it, or a file.
fn pick_preset(family: Family, plugin: &str, want: &Value, program: Option<usize>) -> Result<VendorPreset> {
    let s = match want {
        Value::Number(n) => {
            let list = vendor::list(family, plugin);
            let count = list.len();
            return n.as_u64().and_then(|i| list.into_iter().nth(i as usize)).ok_or_else(|| anyhow!("no preset #{n} ({count} presets)"));
        }
        Value::String(s) => s,
        _ => bail!("preset must be a name, an index or a file"),
    };
    let file = paths::resolve(s);
    if file.extension().is_some() && file.is_file() {
        let name = file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok(VendorPreset { name, group: String::new(), path: file, program });
    }
    let list = vendor::list(family, plugin);
    let want = norm(s);
    let full = |p: &VendorPreset| norm(&format!("{}/{}", p.group, p.name));
    let pick = |found: Vec<&VendorPreset>, how: &str| -> Result<Option<VendorPreset>> {
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(found[0].clone())),
            n => {
                let some: Vec<String> = found.iter().take(8).map(|p| format!("{}/{}", p.group, p.name)).collect();
                bail!("'{s}' {how} {n} presets ({}{}); give pack/name or the index from presets", some.join(", "), if n > 8 { ", ..." } else { "" })
            }
        }
    };
    let exact = list.iter().filter(|p| norm(&p.name) == want || full(p) == want).collect();
    if let Some(p) = pick(exact, "names")? {
        return Ok(p);
    }
    let partial = list.iter().filter(|p| full(p).contains(&want)).collect();
    pick(partial, "matches")?.ok_or_else(|| anyhow!("no {} preset matching '{s}' ({} presets; presets lists them)", family.extension(), list.len()))
}

fn load_preset(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (p, slot) = need_slot(sh, a, "load_preset")?;
    let want: Value = req(a, "preset")?;
    let program = get::<usize>(a, "program")?;
    let keep = get::<bool>(a, "keep_params")?.unwrap_or(false);
    let (preset, state) = vst::with_slot(&p, slot, |inst| {
        let (name, family) = preset_format(&inst.loaded);
        let family = family.ok_or_else(|| no_format(&name))?;
        let preset = pick_preset(family, &name, &want, program)?;
        let current = Preset::from_bytes(&inst.host.state()?).context("reading the plugin's state")?;
        let component = vendor::to_state(family, &current.state.component, &preset)?;
        let probe = vendor::probe_note(family, &component);
        let next = Preset { class_id: current.class_id, state: State { component, controller: vec![] }, info: None };
        // a sample player loads its samples in the background: wait until it plays (its params would be reset too)
        let watch = family.loads_in_background().then(|| inst.host.watch());
        inst.host.set_state(&next.to_bytes()?)?;
        if let Some(w) = watch
            && flux_vst3::settle::wait_playable(&inst.host, &mut inst.port, &w, probe, SAMPLE_LOAD)?.timed_out()
        {
            bail!("{name} hadn't loaded '{}' after {} s (are its samples on a slow or missing drive?)", preset.name, SAMPLE_LOAD.as_secs());
        }
        let state = inst.host.state()?;
        reapply_params(&inst.host, &p, slot, keep)?;
        Ok((preset, state))
    })?;
    let stored = vst::store_state(&state)?;
    let c = Change { preset: Some(stored.clone()), preset_name: Some(preset.name.clone()), clear_params: !keep, ..Change::default() };
    save(sh, slot, c, Source::Agent, None)?;
    text(json!({
        "ok": true, "slot": label(&p, slot), "loaded": preset.name, "pack": preset.group, "file": rel(&preset.path),
        "preset": stored, "params_kept": keep, "revision": sh.lock().session.revision,
    }))
}

// ---- capturing ----------------------------------------------------------------------------------------

/// Save the plugin's current state as the slot's preset. Returns the stored preset, or None if the plugin isn't
/// loaded or already matches the project.
fn capture_slot(sh: &Arc<Shared>, slot: SlotId, keep_params: bool, source: Source) -> Result<Option<String>> {
    let p = sh.lock().session.project.clone();
    let Some(cap) = vst::current_state(&p, slot)? else { return Ok(None) };
    if let Some(cur) = slot_preset(&p, slot)
        && std::fs::read(paths::resolve(&cur)).is_ok_and(|b| b == cap.state)
    {
        return Ok(None);
    }
    let stored = vst::store_state(&cap.state)?;
    save(sh, slot, Change { preset: Some(stored.clone()), clear_params: !keep_params, ..Change::default() }, source, Some(&cap))?;
    Ok(Some(stored))
}

fn capture(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (p, slot) = need_slot(sh, a, "capture")?;
    let keep = get::<bool>(a, "keep_params")?.unwrap_or(false);
    if vst::current_state(&p, slot)?.is_none() {
        // not loaded yet: load it set up from the project, so the capture folds params into the preset
        vst::with_slot(&p, slot, |_| Ok(()))?;
    }
    let stored = capture_slot(sh, slot, keep, Source::Agent)?;
    text(json!({"ok": true, "slot": label(&p, slot), "preset": stored, "changed": stored.is_some(), "revision": sh.lock().session.revision}))
}

// ---- the plugin's window ----------------------------------------------------------------------------

/// A slot whose plugin window is open: the channel its watcher listens on.
struct Watcher {
    id: u64,
    tx: Sender<EditorNotice>,
}

fn watchers() -> &'static Mutex<HashMap<SlotId, Watcher>> {
    static W: OnceLock<Mutex<HashMap<SlotId, Watcher>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A parameter an automation lane can move.
#[derive(Clone, Debug)]
pub struct Automatable {
    /// What a lane names it by: its title, or `#id` where the plugin gives two parameters one title.
    pub name: String,
    pub title: String,
    /// Where it is now, normalized 0..1.
    pub value: f64,
}

/// The parameters of a slot's plugin that automation can move, in the plugin's order (the GUI's lane picker).
pub fn automatable(sh: &Arc<Shared>, slot: SlotId) -> Result<Vec<Automatable>> {
    let p = sh.lock().session.project.clone();
    let all = vst::with_slot(&p, slot, |inst| inst.host.params(None))?;
    let key = |t: &str| t.trim().to_lowercase();
    let mut titles: HashMap<String, usize> = HashMap::new();
    for q in &all {
        *titles.entry(key(&q.title)).or_default() += 1;
    }
    Ok(all
        .iter()
        .filter(|q| q.has("automatable") && !q.has("read_only") && !q.has("hidden"))
        .map(|q| {
            let unique = !q.title.trim().is_empty() && titles[&key(&q.title)] == 1;
            let name = if unique { q.title.trim().to_string() } else { format!("#{}", q.id) };
            Automatable { name, title: q.title.trim().to_string(), value: q.value }
        })
        .collect())
}

/// Show a track's instrument window to the human (the GUI's track header uses this).
pub fn open_track_editor(sh: &Arc<Shared>, track: u64) -> Result<()> {
    let p = sh.lock().session.project.clone();
    vst_track(&p, &TrackRef::Id(track))?;
    open_slot_editor(sh, SlotId::Instrument(track))
}

/// Show a slot's plugin window to the human (also used by the GUI).
pub fn open_slot_editor(sh: &Arc<Shared>, slot: SlotId) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let (p, near) = {
        let st = sh.lock();
        (st.session.project.clone(), st.window)
    };
    let current = watchers().lock().unwrap_or_else(|e| e.into_inner()).get(&slot).map(|w| w.tx.clone());
    let (tx, rx) = match current {
        Some(tx) => (tx, None),
        None => {
            let (tx, rx) = mpsc::channel();
            (tx, Some(rx))
        }
    };
    let ntx = tx.clone();
    let notify: vst::NoticeFn = Arc::new(move |n| {
        let _ = ntx.send(n);
    });
    // (loads the plugin first if needed, which can take a while for big instruments)
    vst::open_editor(&p, slot, near, notify)?;
    if let Some(rx) = rx {
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        watchers().lock().unwrap_or_else(|e| e.into_inner()).insert(slot, Watcher { id, tx });
        let sh2 = sh.clone();
        let name = match slot {
            SlotId::Instrument(t) => format!("plugin-window-track-{t}"),
            SlotId::Insert(i) => format!("plugin-window-insert-{i}"),
        };
        std::thread::Builder::new().name(name).spawn(move || watch(sh2, slot, id, rx))?;
    }
    Ok(())
}

/// Saves what the human changes in a plugin window: shortly after they stop, and when they close it.
fn watch(sh: Arc<Shared>, slot: SlotId, id: u64, rx: mpsc::Receiver<EditorNotice>) {
    let mut edited_at: Option<Instant> = None;
    let save = |sh: &Arc<Shared>| {
        if let Err(e) = capture_slot(sh, slot, false, Source::Human) {
            let what = label(&sh.lock().session.project, slot);
            sh.with_session(|s| s.note(Source::System, format!("couldn't save the plugin window's changes on {what}: {e:#}")));
        }
    };
    loop {
        let wait = edited_at.map_or(Duration::from_millis(1000), |t| SETTLE.saturating_sub(t.elapsed()));
        match rx.recv_timeout(wait) {
            Ok(EditorNotice::Edited) => edited_at = Some(Instant::now()),
            Ok(EditorNotice::Closed) => {
                if edited_at.is_some() {
                    save(&sh);
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if edited_at.is_some_and(|t| t.elapsed() >= SETTLE) {
                    edited_at = None;
                    save(&sh);
                }
                // the plugin crashed, the slot changed plugins or went away, or the project was replaced
                let p = sh.lock().session.project.clone();
                if !vst::editor_open(&p, slot) && edited_at.is_none() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let mut w = watchers().lock().unwrap_or_else(|e| e.into_inner());
    if w.get(&slot).is_some_and(|x| x.id == id) {
        w.remove(&slot);
    }
}

fn open_editor(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (p, slot) = need_slot(sh, a, "open_editor")?;
    open_slot_editor(sh, slot)?;
    text(json!({"ok": true, "slot": label(&p, slot),
        "note": "the window is open on the human's screen; their changes are saved to this slot (source=human) about a second after they stop and when they close it"}))
}

fn close_editor(sh: &Arc<Shared>, a: &Value) -> Result<Vec<Content>> {
    let (p, slot) = need_slot(sh, a, "close_editor")?;
    vst::close_editor(&p, slot)?;
    text(json!({"ok": true}))
}

// ---- installed effects, for menus ------------------------------------------------------------------------

/// An installed VST3 effect.
#[derive(Clone, Debug)]
pub struct EffectEntry {
    pub name: String,
    pub vendor: String,
    /// As stored in the project (relative to the music project when inside it).
    pub path: String,
}

/// Installed VST3 effects by vendor and name. None while the first scan runs: the first call starts it on its own
/// thread and `ready` runs once it is done (a GUI repaints there).
pub fn effects(ready: impl FnOnce() + Send + 'static) -> Option<Arc<Vec<EffectEntry>>> {
    static LIST: Mutex<Option<Arc<Vec<EffectEntry>>>> = Mutex::new(None);
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if let Some(l) = LIST.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(l);
    }
    if !STARTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        let _ = std::thread::Builder::new().name("effect-scan".into()).spawn(move || {
            let found = scanner(false).scan(&scan::find_bundles(&plugin_dirs()));
            let mut list: Vec<EffectEntry> = found
                .iter()
                .filter(|p| p.ok() && !p.is_instrument())
                .map(|p| EffectEntry { name: p.name(), vendor: p.vendor().to_string(), path: rel(&p.path) })
                .collect();
            list.sort_by_cached_key(|e| (e.vendor.to_lowercase(), e.name.to_lowercase()));
            *LIST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(list));
            ready();
        });
    }
    None
}
