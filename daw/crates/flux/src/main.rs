//! Flux: an agent-first DAW. Agents drive it over MCP; humans watch and edit in the same window.

mod audio;
mod gui;
mod mcp;
mod measure;
mod plugins;
mod state;
mod stdio;
mod tools;

use anyhow::{bail, Context, Result};
use flux_core::{Project, Session, Source};
use state::{MainMsg, Shared};

const HELP: &str = "\
Flux - an agent-first DAW

usage:
  flux [FILE]                       open the visual app (FILE: .flux.json, pipeline <title>.json, or .mid)
  flux mcp [--gui] [--project FILE] MCP server on stdio for agents (--gui also opens the window)
  flux render FILE OUT.wav [--stems DIR] [--layers DIR] [--report OUT.json]
                                     render headless to a 16-bit dithered WAV and print the analysis
                                     (--layers: one float WAV per track layer; they sum to the master)
  flux live FILE OUT.wav [--seconds S] [--from-bar B] [--match-render]
                                     run the live playback engine headless (speed + sound check)
  flux import SOURCE OUT.flux.json convert a pipeline song or MIDI file into a Flux project
  flux assets status|verify|fetch|lock|repair|licenses [...]
                                     instruments, tools and plugins (assets.toml): install from official
                                     sources, verify installs, license audit and credits (`flux assets help`)
  flux flac IN.wav OUT.flac [...]   encode FLAC (16-bit, dithered; `flux flac --help`)
  flux sfz check|build|untracked|info|ranges|check-paths|samples|tighten|retune|even [...]
                                     SFZ instruments: build and check the wrapper sets (sfz/sets/), describe
                                     and check instruments, make tightened/retuned/evened copies (`flux sfz help`)

environment: FLUX_ROOT (music project root), FLUX_SFIZZ (sfizz.dll); the BATON_* names still work";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // VST3 plugins run in child processes of this same executable (`flux plugin-host serve`), so a
    // plugin that crashes or hangs can't take Flux down
    if args.first().map(String::as_str) == Some("plugin-host") {
        return flux_vst3::server::run(&args[1..]);
    }
    // offline SFZ and rig renders, one per process (see flux_engine::worker)
    if args.first().map(String::as_str) == Some("render-worker") {
        return flux_engine::worker::main();
    }
    // instrument cards, one SFZ per process (see flux_analysis::card::catalog)
    if args.first().map(String::as_str) == Some("card-worker") {
        return std::process::ExitCode::from(flux_analysis::card::catalog::worker_main(&args[1..]).clamp(0, 255) as u8);
    }
    // these report through their exit code: 0 ok, 1 problems found, 2 couldn't run
    match args.first().map(String::as_str) {
        Some("assets") => return flux_assets::cli::assets_main(flux_engine::paths::root(), &args[1..]),
        Some("flac") => return flux_assets::cli::flac_main(&args[1..]),
        Some("sfz") => return flux_sfz::cli::sfz_main(flux_engine::paths::root(), &args[1..]),
        _ => {}
    }
    match real_main(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("flux: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn real_main(args: Vec<String>) -> Result<()> {
    flux_engine::vst::set_host_command(flux_engine::vst::HostCommand { exe: std::env::current_exe().ok(), args: vec!["plugin-host".into()] });
    if let Ok(exe) = std::env::current_exe() {
        flux_engine::worker::set_command(flux_engine::worker::WorkerCommand { exe, args: vec![] });
    }
    match args.first().map(String::as_str) {
        None => gui::run(Shared::new(open_session(None)?)),
        Some("mcp") => run_mcp(&args[1..]),
        Some("gui") => gui::run(Shared::new(open_session(args.get(1).map(String::as_str))?)),
        Some("render") => cli_render(&args[1..]),
        Some("live") => cli_live(&args[1..]),
        Some("import") => cli_import(&args[1..]),
        Some("help" | "-h" | "--help") => {
            println!("{HELP}");
            Ok(())
        }
        Some(f) if !f.starts_with('-') && std::path::Path::new(f).exists() => gui::run(Shared::new(open_session(Some(f))?)),
        Some(other) => bail!("unknown command '{other}'\n\n{HELP}"),
    }
}

fn open_session(path: Option<&str>) -> Result<Session> {
    let Some(p) = path else {
        let mut s = Session::new(Project::new("untitled", 120.0, 16));
        s.note(Source::System, "new project".into());
        return Ok(s);
    };
    let path = flux_engine::paths::resolve(p);
    let (project, native) = tools::load_any(&path)?;
    let mut s = Session::new(project);
    if native {
        s.path = Some(path.clone());
    }
    s.note(Source::System, format!("opened {}", path.display()));
    Ok(s)
}

fn run_mcp(args: &[String]) -> Result<()> {
    // before anything can load sfizz, which prints to stdout
    let out = stdio::take_stdout();
    let (mut gui, mut project) = (false, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gui" => gui = true,
            "--project" => {
                i += 1;
                project = Some(args.get(i).context("--project needs a file")?.clone());
            }
            x => bail!("unknown option '{x}'"),
        }
        i += 1;
    }
    let sh = Shared::new(open_session(project.as_deref())?);
    let (tx, rx) = std::sync::mpsc::channel();
    *sh.main_tx.lock() = Some(tx.clone());
    let server = sh.clone();
    std::thread::Builder::new().name("mcp".into()).spawn(move || {
        mcp::serve(server, out);
        let _ = tx.send(MainMsg::Quit);
    })?;
    // the window must run on the main thread; it can be closed and reopened (gui action=open)
    if gui
        && let Err(e) = gui::run(sh.clone()) {
            eprintln!("flux: {e:#}");
        }
    while let Ok(m) = rx.recv() {
        match m {
            MainMsg::OpenGui => {
                if let Err(e) = gui::run(sh.clone()) {
                    eprintln!("flux: {e:#}");
                }
            }
            MainMsg::Quit => break,
        }
    }
    std::process::exit(0)
}

fn cli_render(args: &[String]) -> Result<()> {
    let (Some(src), Some(dst)) = (args.first(), args.get(1)) else { bail!("usage: flux render FILE OUT.wav [--stems DIR] [--report OUT.json]") };
    let (mut stems, mut report, mut layers) = (None, None, None);
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--stems" => {
                i += 1;
                stems = Some(flux_engine::paths::resolve(args.get(i).context("--stems needs a directory")?));
            }
            "--report" => {
                i += 1;
                report = Some(flux_engine::paths::resolve(args.get(i).context("--report needs a file")?));
            }
            "--layers" => {
                i += 1;
                layers = Some(flux_engine::paths::resolve(args.get(i).context("--layers needs a directory")?));
            }
            x => bail!("unknown option '{x}'"),
        }
        i += 1;
    }
    let (project, _) = tools::load_any(&flux_engine::paths::resolve(src))?;
    let progress: flux_engine::render::Progress = std::sync::Arc::new(|m: String| eprintln!("  {m}"));
    let opts = flux_engine::RenderOptions { stems_dir: stems, progress: Some(progress), layers: layers.is_some(), ..Default::default() };
    let out = flux_engine::render(&project, &opts)?;
    let dst = flux_engine::paths::resolve(dst);
    let layer_stems = if layers.is_some() { out.exportable_layers()? } else { &[] };
    flux_engine::render::export_wav(&out, &dst, 1)?;
    if let Some(dir) = &layers {
        for (name, buf) in layer_stems {
            flux_engine::render::write_wav_f32(&dir.join(format!("{name}.wav")), out.sr, buf)?;
        }
        eprintln!("layers {:?}: sum differs from the master by at most {:.1} dBFS", out.layers.iter().map(|l| &l.0).collect::<Vec<_>>(),
            out.layer_sum_error_db.unwrap_or(f64::NAN));
    }
    let analysis = flux_engine::analysis::all(&project, &out)?;
    let text = serde_json::to_string_pretty(&analysis)?;
    if let Some(r) = report {
        std::fs::write(&r, &text)?;
    }
    eprintln!("wrote {} ({:.1} s, {:.1} LUFS, {:.2} dBTP) in {:.1} s", dst.display(), out.duration_s(), out.lufs, out.true_peak_dbtp, out.render_seconds);
    println!("{text}");
    Ok(())
}

fn cli_live(args: &[String]) -> Result<()> {
    use flux_engine::live::{LiveEngine, RenderHints};
    let (Some(src), Some(dst)) = (args.first(), args.get(1)) else {
        bail!("usage: flux live FILE OUT.wav [--seconds S] [--from-bar B] [--match-render]")
    };
    let (mut seconds, mut from_bar, mut match_render) = (30.0f64, 1u32, false);
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => {
                i += 1;
                seconds = args.get(i).context("--seconds needs a number")?.parse()?;
            }
            "--from-bar" => {
                i += 1;
                from_bar = args.get(i).context("--from-bar needs a bar")?.parse()?;
            }
            "--match-render" => match_render = true,
            x => bail!("unknown option '{x}'"),
        }
        i += 1;
    }
    let (project, _) = tools::load_any(&flux_engine::paths::resolve(src))?;
    let hints = if match_render {
        let out = flux_engine::render(&project, &flux_engine::RenderOptions::default())?;
        eprintln!("offline render: {:.1} LUFS, drive {:+.1} dB", out.lufs, out.drive_db);
        RenderHints::from_render(&out)
    } else {
        RenderHints::default()
    };
    let sr = project.sample_rate;
    let tm = flux_core::TimeMap::new(&project);
    let start = (tm.sec(tm.bar_beat(from_bar.max(1))) * sr as f64) as usize;
    let t0 = std::time::Instant::now();
    let mut eng = LiveEngine::new(std::sync::Arc::new(project), hints);
    while (eng.loading() > 0 || eng.plugins_loading() > 0) && t0.elapsed().as_secs() < 120 {
        eng.poll();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let load_s = t0.elapsed().as_secs_f64();
    for (id, s) in eng.status() {
        if s != "live" {
            eprintln!("  track {id}: {s}");
        }
    }
    eng.seek(start);
    let total = (seconds * sr as f64) as usize;
    let (mut l, mut r) = (vec![0f32; total], vec![0f32; total]);
    let mut pos = vec![0u32; 1024];
    let t1 = std::time::Instant::now();
    let mut done = 0;
    while done < total {
        let n = 1024.min(total - done);
        let got = eng.render(&mut l[done..done + n], &mut r[done..done + n], &mut pos[..n]);
        done += got;
        if got < n {
            break;
        }
    }
    let secs = t1.elapsed().as_secs_f64();
    l.truncate(done);
    r.truncate(done);
    let dst = flux_engine::paths::resolve(dst);
    flux_engine::render::write_wav_f32(&dst, sr, &(l.clone(), r.clone()))?;
    let (lufs, tp) = flux_engine::render::measure(&l, &r, sr)?;
    eprintln!(
        "live: loaded instruments in {load_s:.1} s; rendered {:.1} s of audio in {secs:.2} s ({:.1}x realtime); {lufs:.1} LUFS, {tp:.2} dBTP -> {}",
        done as f64 / sr as f64,
        done as f64 / sr as f64 / secs.max(1e-9),
        dst.display()
    );
    Ok(())
}

fn cli_import(args: &[String]) -> Result<()> {
    let (Some(src), Some(dst)) = (args.first(), args.get(1)) else { bail!("usage: flux import SOURCE OUT.flux.json") };
    let (project, _) = tools::load_any(&flux_engine::paths::resolve(src))?;
    let dst = flux_engine::paths::resolve(dst);
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&dst, serde_json::to_string_pretty(&project)?)?;
    eprintln!("wrote {} ({} tracks, {} bars)", dst.display(), project.tracks.len(), project.bars);
    if !project.notes.is_empty() {
        eprintln!("{}", project.notes);
    }
    Ok(())
}
