//! `flux-analyze`: the analyses of `flux-analysis` from the command line, on WAV and FLAC files. Every
//! command prints JSON (the same structures the library returns) unless `--text` asks for the human-readable
//! tables of the Python tools it replaces. See `flux-analyze.txt` (or `--help`) for the commands.

use anyhow::{Context, Result, bail, ensure};
use flux_analysis::audio::{Audio, AudioRef};
use flux_analysis::loudness::{GameOptions, check_game};
use flux_analysis::meta::{LayersManifest, PipelineMeta, Section};
use flux_analysis::mix::{MixOptions, check_mix};
use flux_analysis::reference::{ReferenceOptions, compare};
use flux_analysis::spectro::{SpectroOptions, spectrogram};
use flux_analysis::stems::stem_balance;
use flux_analysis::timefx::{tempo_blocks, time_stop_report};
use serde::Serialize;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("flux-analyze: {e:#}");
            std::process::exit(2);
        }
    }
}

/// Parsed options: positional arguments plus `--flag [value]` pairs.
struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
}

const SWITCHES: [&str; 6] = ["--text", "--recursive", "--no-fold", "--no-loops", "--json", "--help"];

impl Args {
    fn parse(a: &[String]) -> Result<Args> {
        let (mut pos, mut opts) = (vec![], vec![]);
        let mut it = a.iter();
        while let Some(x) = it.next() {
            if x.starts_with("--") || (x.starts_with('-') && x.len() == 2 && !x[1..].starts_with(|c: char| c.is_ascii_digit())) {
                if SWITCHES.contains(&x.as_str()) {
                    opts.push((x.clone(), None));
                } else {
                    let v = it.next().with_context(|| format!("{x} needs a value"))?;
                    opts.push((x.clone(), Some(v.clone())));
                }
            } else {
                pos.push(x.clone());
            }
        }
        Ok(Args { pos, opts })
    }

    fn flag(&self, name: &str) -> bool {
        self.opts.iter().any(|(k, _)| k == name)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
    }

    fn num<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>>
    where
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        self.get(name).map(|v| v.parse::<T>().with_context(|| format!("{name} {v:?}"))).transpose()
    }

    fn check(&self, known: &[&str]) -> Result<()> {
        for (k, _) in &self.opts {
            ensure!(known.contains(&k.as_str()) || k == "--text" || k == "--json", "unknown option {k} (see --help)");
        }
        Ok(())
    }
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn read(p: &str) -> Result<Audio> {
    Audio::read(Path::new(p))
}

/// Sections from a `.layers.json` manifest, the pipeline's song metadata, or a JSON list of
/// `{"name", "start_s", "end_s"}`.
fn load_sections(p: &str) -> Result<Vec<Section>> {
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("parsing {p}"))?;
    if v.is_array() {
        return serde_json::from_value(v).with_context(|| format!("{p}: expected [{{\"name\", \"start_s\", \"end_s\"}}, ...]"));
    }
    if v.get("layers").is_some() {
        return Ok(LayersManifest::read(Path::new(p))?.sections());
    }
    if v.get("loop_length_s").is_some() {
        return PipelineMeta::read(Path::new(p))?.sections();
    }
    bail!("{p} is neither a layers manifest, the pipeline's song metadata, nor a list of sections")
}

/// Stems from a manifest's layer files, or every WAV/FLAC in a directory.
fn load_stems(manifest: Option<&str>, dir: Option<&str>) -> Result<Vec<(String, Audio)>> {
    use rayon::prelude::*;
    let mut files: Vec<(String, PathBuf)> = vec![];
    if let Some(m) = manifest {
        files.extend(LayersManifest::read(Path::new(m))?.layer_paths(Path::new(m)));
    }
    if let Some(d) = dir {
        let mut found: Vec<PathBuf> = std::fs::read_dir(d)
            .with_context(|| format!("listing {d}"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("flac")))
            .collect();
        found.sort();
        files.extend(found.into_iter().map(|f| (f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), f)));
    }
    // decoding is the slow part with many stems: read them in parallel
    files.into_par_iter().map(|(name, path)| Ok((name, Audio::read(&path)?))).collect()
}

/// Runs a command; the exit code is 1 when `loudness` finds an error (a flag of severity "error" or an
/// unreadable file), else 0.
fn run(args: &[String]) -> Result<i32> {
    let Some(cmd) = args.first() else {
        println!("{}", include_str!("flux-analyze.txt"));
        return Ok(0);
    };
    let a = Args::parse(&args[1..])?;
    if cmd == "-h" || cmd == "--help" || a.flag("--help") {
        println!("{}", include_str!("flux-analyze.txt"));
        return Ok(0);
    }
    let mut code = 0;
    match cmd.as_str() {
        "spectro" => {
            a.check(&["--fft", "--width", "--height"])?;
            ensure!(a.pos.len() >= 4, "usage: flux-analyze spectro OUT.png START_S END_S A.wav [B.wav ...]");
            let t0: f64 = a.pos[1].parse().context("START_S")?;
            let t1: f64 = a.pos[2].parse().context("END_S")?;
            let mut o = SpectroOptions::default();
            if let Some(n) = a.num("--fft")? {
                o.fft_size = n;
            }
            if let Some(w) = a.num("--width")? {
                o.width = w;
            }
            if let Some(h) = a.num("--height")? {
                o.height = h;
            }
            let audio = a.pos[3..].iter().map(|p| read(p)).collect::<Result<Vec<_>>>()?;
            let views: Vec<AudioRef> = audio.iter().map(Audio::view).collect();
            spectrogram(&views, t0, t1, &o)?.write_png(Path::new(&a.pos[0]))?;
            println!("{}", a.pos[0]);
        }
        "stems" => {
            a.check(&["--meta", "--manifest", "--dir", "--sections", "--loop-s", "--no-fold"])?;
            let (sections, loop_s, stems) = if let Some(m) = a.get("--meta") {
                let meta = PipelineMeta::read(Path::new(m))?;
                let default_dir = Path::new(m).parent().unwrap_or(Path::new(".")).join("stems");
                let dir = a.get("--dir").map(PathBuf::from).unwrap_or(default_dir);
                (meta.sections()?, Some(meta.loop_length_s), load_stems(None, Some(&dir.to_string_lossy()))?)
            } else if let Some(m) = a.get("--manifest") {
                let man = LayersManifest::read(Path::new(m))?;
                // exported layers are already folded to the loop length
                (man.sections(), None, load_stems(Some(m), None)?)
            } else {
                let s = a.get("--sections").context("stems needs --meta, --manifest, or --sections with --dir")?;
                (load_sections(s)?, a.num::<f64>("--loop-s")?, load_stems(None, a.get("--dir"))?)
            };
            ensure!(!stems.is_empty(), "no stems found");
            let sr = stems[0].1.sr;
            let fold = if a.flag("--no-fold") { None } else { loop_s.map(|s| (s * sr as f64).round() as usize) };
            let views: Vec<(&str, AudioRef)> = stems.iter().map(|(n, x)| (n.as_str(), x.view())).collect();
            let t = stem_balance(&views, &sections, fold)?;
            if a.flag("--text") { print!("{}", t.table()) } else { print_json(&t)? }
        }
        "timefx" => {
            a.check(&["--meta", "--audio", "--step"])?;
            let m = a.get("--meta").context("timefx needs --meta output/<title>/<title>.json")?;
            let meta = PipelineMeta::read(Path::new(m))?;
            let audio_path = match a.get("--audio") {
                Some(p) => PathBuf::from(p),
                None => Path::new(m).parent().unwrap_or(Path::new(".")).join(format!("{}_loop.wav", meta.title)),
            };
            let audio = Audio::read(&audio_path)?;
            let stops = meta.timestops.iter().map(|t| time_stop_report(audio.view(), t)).collect::<Result<Vec<_>>>()?;
            let tm = meta.tempo()?;
            let blocks = match meta.tempo_map.as_ref().and_then(|m| m.get(1)) {
                Some(first) => tempo_blocks(audio.view(), &tm, first.bar, meta.bars, a.num("--step")?.unwrap_or(4))?,
                None => vec![],
            };
            if a.flag("--text") {
                for (r, t) in stops.iter().zip(&meta.timestops) {
                    println!("TIME STOP ({})", t.bars.clone().unwrap_or_default());
                    println!("  before the stop   {:6.1} dB   centroid {:6.0} Hz", r.before.rms_db, r.before.centroid_hz);
                    for (i, w) in r.tape_stop.iter().enumerate() {
                        println!("  tape stop {}/4    {:6.1} dB   centroid {:6.0} Hz", i + 1, w.rms_db, w.centroid_hz);
                    }
                    println!("  frozen (3 bars)   {:6.1} dB   centroid {:6.0} Hz", r.frozen.rms_db, r.frozen.centroid_hz);
                    println!("  resume hit (1 s)  {:6.1} dB", r.resume_hit_db);
                }
                if !blocks.is_empty() {
                    println!("ACCELERANDO (audio onset-autocorrelation tempo per block vs written)");
                    for b in &blocks {
                        let m = b.measured_bpm.map(|v| format!("{v:6.1}")).unwrap_or_else(|| "     -".into());
                        println!(
                            "  bars {:2}-{:2}: written {}-{} (avg {:.0})   measured {m}",
                            b.first_bar, b.last_bar, b.written_min, b.written_max, b.written_avg
                        );
                    }
                }
            } else {
                print_json(&serde_json::json!({"time_stops": stops, "tempo_blocks": blocks}))?;
            }
        }
        "loudness" => {
            a.check(&["--recursive", "--no-loops", "--target", "--tolerance", "--ceiling", "--max-bytes", "-j", "--jobs"])?;
            ensure!(!a.pos.is_empty(), "usage: flux-analyze loudness PATH [PATH ...]");
            let mut o = GameOptions { recursive: a.flag("--recursive"), detect_loops: !a.flag("--no-loops"), ..Default::default() };
            if let Some(v) = a.num("--target")? {
                o.target_lufs = v;
            }
            if let Some(v) = a.num("--tolerance")? {
                o.lufs_tolerance = v;
            }
            if let Some(v) = a.num("--ceiling")? {
                o.ceiling_dbtp = v;
            }
            if let Some(v) = a.num("--max-bytes")? {
                o.max_file_bytes = v;
            }
            if let Some(v) = a.num("-j")?.or(a.num("--jobs")?) {
                o.jobs = v;
            }
            let paths: Vec<PathBuf> = a.pos.iter().map(PathBuf::from).collect();
            let rep = check_game(&paths, &o)?;
            if a.flag("--text") {
                println!("{:40} {:>7} {:>5} {:>7} {:>7} {:>6}  flags", "track", "LUFS", "LRA", "dBTP", "ST max", "PLR");
                let f = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into());
                for t in &rep.tracks {
                    let s = &t.stats;
                    let codes: Vec<&str> = t.flags.iter().map(|f| f.code).collect();
                    println!(
                        "{:40} {:>7} {:>5} {:>7.1} {:>7} {:>6}  {}",
                        t.name,
                        f(s.integrated_lufs),
                        f(s.lra_lu),
                        s.true_peak_dbtp,
                        f(s.short_term_max_lufs),
                        f(s.plr_db),
                        codes.join(" ")
                    );
                }
                for u in &rep.unreadable {
                    println!("unreadable: {}: {}", u.path.display(), u.error);
                }
            } else {
                print_json(&rep)?;
            }
            if rep.summary.errors > 0 || !rep.unreadable.is_empty() {
                code = 1;
            }
        }
        "reference" => {
            a.check(&["--sections", "--ref-sections", "--manifest", "--stems", "--max"])?;
            ensure!(a.pos.len() == 2, "usage: flux-analyze reference MIX REFERENCE [options]");
            let mix = read(&a.pos[0])?;
            let rf = read(&a.pos[1])?;
            let stems = load_stems(a.get("--manifest"), a.get("--stems"))?;
            let views: Vec<(&str, AudioRef)> = stems.iter().map(|(n, x)| (n.as_str(), x.view())).collect();
            let o = ReferenceOptions {
                mix_sections: a.get("--sections").map(load_sections).transpose()?.unwrap_or_default(),
                reference_sections: a.get("--ref-sections").map(load_sections).transpose()?.unwrap_or_default(),
                section_suggestions: a.num("--max")?.unwrap_or(5),
            };
            let rep = compare(mix.view(), rf.view(), &views, &o)?;
            if a.flag("--text") {
                println!("{} against {}", a.pos[0], a.pos[1]);
                for s in &rep.whole.suggestions {
                    println!("  [{:4.1}] {:10} {}", s.score, s.area, s.message);
                }
                for s in &rep.sections {
                    println!("{} ({:.1}-{:.1} s) against {}:", s.section, s.start_s, s.end_s, s.against);
                    for x in &s.suggestions {
                        println!("  [{:4.1}] {:10} {}", x.score, x.area, x.message);
                    }
                }
            } else {
                print_json(&rep)?;
            }
        }
        "mix" => {
            a.check(&["--sections", "--manifest", "--stems", "--dry", "--max"])?;
            ensure!(a.pos.len() == 1, "usage: flux-analyze mix MIX [--manifest X.layers.json | --stems DIR] [--dry FILE] [--sections FILE]");
            let mix = read(&a.pos[0])?;
            let stems = load_stems(a.get("--manifest"), a.get("--stems"))?;
            let views: Vec<(&str, AudioRef)> = stems.iter().map(|(n, x)| (n.as_str(), x.view())).collect();
            let dry = a.get("--dry").map(read).transpose()?;
            let mut sections = a.get("--sections").map(load_sections).transpose()?.unwrap_or_default();
            if sections.is_empty()
                && let Some(m) = a.get("--manifest")
            {
                sections = LayersManifest::read(Path::new(m))?.sections();
            }
            let o = MixOptions { sections, max_conflicts: a.num("--max")?.unwrap_or(12), ..Default::default() };
            let rep = check_mix(mix.view(), &views, dry.as_ref().map(Audio::view), &o)?;
            if a.flag("--text") {
                for c in &rep.conflicts {
                    println!("[{:5.1}] {}", c.score, c.message);
                }
                for b in &rep.buried {
                    println!("buried: {}", b.message);
                }
                for t in &rep.tone {
                    println!(
                        "{:20} mud {:>6} (pink {:>6})  harsh {:>6} (pink {:>6})  {}",
                        t.section,
                        t.mud_db.map(|v| format!("{v:.1}")).unwrap_or_default(),
                        t.mud_vs_pink_db.map(|v| format!("{v:+.1}")).unwrap_or_default(),
                        t.harsh_db.map(|v| format!("{v:.1}")).unwrap_or_default(),
                        t.harsh_vs_pink_db.map(|v| format!("{v:+.1}")).unwrap_or_default(),
                        t.flags.join("; ")
                    );
                }
                let f = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into());
                for m in &rep.mono {
                    println!(
                        "{:20} mono fold-down {:>5} LU  corr {:>5}  low-end side {:>6} dB  {}",
                        m.section,
                        f(m.fold_down_lu),
                        m.correlation.map(|x| format!("{x:.2}")).unwrap_or_else(|| "-".into()),
                        f(m.low_end_side_db),
                        m.flags.join("; ")
                    );
                }
                for t in &rep.transients {
                    println!(
                        "{:20} onsets {:5.2}/s  contrast {:>5} dB  tail {:>5} dB  smear {:>5} dB  {}",
                        t.section,
                        t.onsets_per_s,
                        f(t.contrast_db),
                        f(t.tail_db),
                        f(t.smear_db),
                        t.flags.join("; ")
                    );
                }
            } else {
                print_json(&rep)?;
            }
        }
        "profile" => {
            a.check(&["--sections"])?;
            ensure!(a.pos.len() == 1, "usage: flux-analyze profile FILE [--sections FILE]");
            let x = read(&a.pos[0])?;
            let sections = a.get("--sections").map(load_sections).transpose()?.unwrap_or_default();
            flux_analysis::meta::check_sections(&sections, x.view().duration_s())?;
            let mut out = vec![("all".to_string(), flux_analysis::profile::profile(x.view())?)];
            for s in sections {
                out.push((s.name.clone(), flux_analysis::profile::profile(x.view().section(&s)).with_context(|| format!("section '{}'", s.name))?));
            }
            print_json(&out)?;
        }
        _ => bail!("unknown command {cmd:?} (spectro, stems, timefx, loudness, reference, mix, profile; see --help)"),
    }
    Ok(code)
}
