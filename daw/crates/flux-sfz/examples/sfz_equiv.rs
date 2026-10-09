//! Prove that regenerated wrappers play like the old ones: compare what the parser makes of both, and what
//! sfizz renders from both. Used by the verification in `build/flux-sfz_check/`.
//!
//! ```text
//! sfz_equiv regions ROOT GEN_DIR REF_DIR OUT.json
//! sfz_equiv render  ROOT GEN_DIR REF_DIR ref|new OUT.jsonl
//! sfz_equiv diff    REF.jsonl NEW.jsonl
//! ```
//!
//! The wrappers are the `load` files of the manifests `sfz_sets build` wrote into GEN_DIR; each is paired with
//! the file at the same relative path under REF_DIR (`build/sfz`).
//!
//! `regions` loads both files of every pair and compares region by region: the sample file each region plays
//! (resolved to an absolute path), the interpreted properties, and the effective opcodes apart from
//! `default_path` (which differs by design) and `sw_label` (new).
//!
//! `render` plays a fixed script through sfizz for one side of every pair and writes, per test, a SHA-256 of
//! the audio and its loudness envelope. The script comes from the *new* file on both sides (keys spread over
//! its note range, every keyswitch, two velocities, two CC1 values), so both runs make exactly the same calls.
//! Run it once per side, each in its own process: sfizz's random generators (round robins, `offset_random`)
//! then follow the same sequence. `diff` compares the two outputs.

use flux_engine::sfizz::{Ev, Synth};
use flux_sfz::manifest::Manifest;
use flux_sfz::props::Trigger;
use flux_sfz::set::Wrap;
use flux_sfz::{Instrument, SampleRef};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const SR: u32 = 48_000;
const BLOCK: usize = 256;
/// Each test: setup (CC1, keyswitch), then the note for `NOTE_S`, then the release, `TEST_S` in all.
const SETUP_FRAMES: usize = 2_400;
const NOTE_S: f64 = 0.4;
const TEST_S: f64 = 0.6;
const ENV_WINDOW: usize = 960;

type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let r = match a.first().map(String::as_str) {
        Some("regions") if a.len() == 5 => regions(&a[1], &a[2], &a[3], &a[4]),
        Some("render") if a.len() == 6 => render(&a[1], &a[2], &a[3], &a[4], &a[5]),
        Some("diff") if a.len() == 3 => diff(&a[1], &a[2]),
        _ => Err("usage: sfz_equiv regions ROOT GEN REF OUT.json | render ROOT GEN REF ref|new OUT.jsonl | diff A.jsonl B.jsonl".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sfz_equiv: {e}");
            ExitCode::FAILURE
        }
    }
}

/// (relative path, new file, reference file) for every generated wrapper.
fn pairs(root: &str, gen_dir: &str, ref_dir: &str) -> Res<Vec<(String, PathBuf, PathBuf)>> {
    let root = std::path::absolute(root)?;
    let gen_dir = std::path::absolute(gen_dir)?;
    let mut manifests = vec![];
    let mut stack = vec![gen_dir.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with(".manifest.json") {
                manifests.push(p);
            }
        }
    }
    manifests.sort();
    let mut out = vec![];
    for m in manifests {
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(&m)?)?;
        for e in manifest.instruments.iter().filter(|e| e.wrap != Wrap::None) {
            let new = root.join(&e.load);
            let rel = new.strip_prefix(&gen_dir)?.to_string_lossy().replace('\\', "/");
            out.push((rel.clone(), new, Path::new(ref_dir).join(&rel)));
        }
    }
    Ok(out)
}

fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn regions(root: &str, gen_dir: &str, ref_dir: &str, out: &str) -> Res {
    let mut report = vec![];
    let (mut total, mut bad_files) = (0usize, 0usize);
    for (rel, new, old) in pairs(root, gen_dir, ref_dir)? {
        let a = Instrument::load(&old)?;
        let b = Instrument::load(&new)?;
        let mut problems: Vec<String> = vec![];
        if a.regions().len() != b.regions().len() {
            problems.push(format!("{} regions vs {}", a.regions().len(), b.regions().len()));
        }
        for (ra, rb) in a.regions().iter().zip(b.regions()) {
            let sample = |r: &flux_sfz::Region| match r.sample() {
                SampleRef::File(f) => norm(&f.path),
                SampleRef::Generator { name } => name.clone(),
                SampleRef::None => String::new(),
            };
            if sample(ra) != sample(rb) {
                problems.push(format!("region {}: sample {} vs {}", ra.index(), sample(ra), sample(rb)));
            }
            let mut pa = ra.props().clone();
            let mut pb = rb.props().clone();
            pa.sw_label = None;
            pb.sw_label = None;
            if pa != pb {
                problems.push(format!("region {}: properties differ", ra.index()));
            }
            let ops = |r: &flux_sfz::Region| -> Vec<(String, String)> {
                r.effective()
                    .into_iter()
                    .filter(|(_, o)| o.canonical != "default_path" && o.canonical != "sw_label")
                    .map(|(_, o)| (o.canonical.clone(), o.value.clone()))
                    .collect()
            };
            if ops(ra) != ops(rb) {
                problems.push(format!("region {}: effective opcodes differ", ra.index()));
            }
        }
        // the <control> settings (CC defaults, labels) besides default_path
        let ctl = |i: &Instrument| format!("{:?}", (&i.control().set_cc, &i.control().label_cc, &i.control().label_key, i.control().note_offset, i.control().octave_offset));
        if ctl(&a) != ctl(&b) {
            problems.push("control settings differ".into());
        }
        let errors = |i: &Instrument| i.errors().count();
        if errors(&a) != errors(&b) {
            problems.push(format!("{} errors vs {}", errors(&a), errors(&b)));
        }
        total += b.regions().len();
        if !problems.is_empty() {
            bad_files += 1;
        }
        println!("{rel}: {} regions, {}", b.regions().len(), if problems.is_empty() { "same".to_owned() } else { problems[..problems.len().min(5)].join("; ") });
        report.push(json!({ "file": rel, "regions": b.regions().len(), "problems": problems }));
    }
    println!("{} files, {total} regions, {bad_files} files differ", report.len());
    std::fs::write(out, serde_json::to_string_pretty(&report)?)?;
    Ok(())
}

/// The test script of a wrapper: (keyswitch, key, velocity, cc1).
fn script(inst: &Instrument) -> Vec<(Option<u8>, u8, u8, u8)> {
    let mut keys = BTreeSet::new();
    let mut switches = BTreeSet::new();
    for r in inst.playable_regions() {
        let p = r.props();
        if !p.is_note_on() || p.trigger == Trigger::Legato {
            continue;
        }
        keys.insert(p.keycenter().unwrap_or(p.lokey).clamp(p.lokey, p.hikey));
        if let Some((lo, _)) = p.sw_last {
            switches.insert(lo);
        }
    }
    let keys: Vec<u8> = keys.into_iter().collect();
    let picked: BTreeSet<u8> = if keys.is_empty() { BTreeSet::new() } else { [keys[0], keys[keys.len() / 2], keys[keys.len() - 1]].into() };
    let switches: Vec<Option<u8>> = if switches.is_empty() { vec![None] } else { switches.into_iter().map(Some).collect() };
    let mut tests = vec![];
    for ks in &switches {
        for &key in &picked {
            for vel in [40, 110] {
                for cc1 in [30, 110] {
                    tests.push((*ks, key, vel, cc1));
                }
            }
        }
    }
    tests
}

fn render(root: &str, gen_dir: &str, ref_dir: &str, side: &str, out: &str) -> Res {
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    for (rel, new, old) in pairs(root, gen_dir, ref_dir)? {
        let tests = script(&Instrument::load(&new)?);
        let file = match side {
            "ref" => &old,
            "new" => &new,
            _ => return Err(format!("unknown side `{side}`").into()),
        };
        let mut synth = Synth::new(file, SR, BLOCK).map_err(|e| e.to_string())?;
        let frames = (TEST_S * f64::from(SR)) as usize;
        let off = (NOTE_S * f64::from(SR)) as u64;
        for (ks, key, vel, cc1) in tests {
            synth.all_sound_off();
            let mut setup = vec![(0, Ev::Cc(1, cc1))];
            if let Some(k) = ks {
                setup.push((16, Ev::On(k, 100)));
                setup.push((32, Ev::Off(k)));
            }
            let _ = synth.render(&setup, SETUP_FRAMES);
            let (l, r) = synth.render(&[(0, Ev::On(key, vel)), (off, Ev::Off(key))], frames);
            let mut h = Sha256::new();
            for (a, b) in l.iter().zip(&r) {
                h.update(a.to_le_bytes());
                h.update(b.to_le_bytes());
            }
            let hash: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            let env: Vec<f64> = l
                .chunks(ENV_WINDOW)
                .zip(r.chunks(ENV_WINDOW))
                .map(|(a, b)| {
                    let e: f64 = a.iter().chain(b).map(|x| f64::from(*x).powi(2)).sum::<f64>() / (2 * a.len()) as f64;
                    (10.0 * e.max(1e-20).log10() * 100.0).round() / 100.0
                })
                .collect();
            let id = format!("{rel}|ks={}|key={key}|vel={vel}|cc1={cc1}", ks.map_or("-".into(), |k| k.to_string()));
            writeln!(w, "{}", json!({ "id": id, "regions": synth.num_regions, "sha256": hash, "env_db": env }))?;
        }
    }
    w.flush()?;
    Ok(())
}

fn read_jsonl(path: &str) -> Res<Vec<Value>> {
    let f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut out = vec![];
    for line in f.lines() {
        out.push(serde_json::from_str(&line?)?);
    }
    Ok(out)
}

fn diff(a: &str, b: &str) -> Res {
    let (a, b) = (read_jsonl(a)?, read_jsonl(b)?);
    if a.len() != b.len() {
        return Err(format!("{} tests vs {}", a.len(), b.len()).into());
    }
    let (mut identical, mut close, mut silent) = (0, 0, 0);
    let mut different = vec![];
    for (x, y) in a.iter().zip(&b) {
        if x["id"] != y["id"] {
            return Err(format!("test order differs: {} vs {}", x["id"], y["id"]).into());
        }
        let env = |v: &Value| -> Vec<f64> { v["env_db"].as_array().map(|e| e.iter().filter_map(Value::as_f64).collect()).unwrap_or_default() };
        let (ex, ey) = (env(x), env(y));
        if ex.iter().all(|d| *d < -150.0) {
            silent += 1;
        }
        if x["sha256"] == y["sha256"] && x["regions"] == y["regions"] {
            identical += 1;
            continue;
        }
        // loud windows only: the envelope may differ where a random offset or round robin picks another take
        let worst = ex.iter().zip(&ey).filter(|(p, q)| p.max(**q) > -60.0).map(|(p, q)| (p - q).abs()).fold(0.0, f64::max);
        if worst <= 0.5 && x["regions"] == y["regions"] {
            close += 1;
        } else {
            different.push(format!("{} (regions {} vs {}, worst window {worst:.2} dB)", x["id"], x["regions"], y["regions"]));
        }
    }
    println!(
        "{} tests: {identical} bit-identical, {close} within 0.5 dB per 20 ms window, {} different ({silent} silent on the reference side)",
        a.len(),
        different.len()
    );
    for d in different.iter().take(40) {
        println!("  {d}");
    }
    Ok(())
}
