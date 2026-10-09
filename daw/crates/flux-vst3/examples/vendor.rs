//! List a plugin's presets in its own format, or load one and hear what it does.
//!
//!     cargo run -p flux-vst3 --example vendor -- PLUGIN.vst3 list
//!     cargo run -p flux-vst3 --example vendor -- PLUGIN.vst3 load PRESET [PROGRAM] [PARAM...]
//!     cargo run -p flux-vst3 --example vendor -- PLUGIN.vst3 raw STATE.bin [PARAM...]
//!
//! `load` builds the component state for PRESET (a file, or `#N` for the Nth listed preset) from the plugin's current
//! state, sets it, pumps window messages for a while (some plugins load samples on a timer), prints the changed
//! parameters (or the named ones), then plays C4 for one second (an effect gets a noise burst) and reports the level.
//! The built state is also written to `flux-vendor-last.bin` in the temp folder.

use flux_vst3::vendor::{self, VendorPreset};
use flux_vst3::{Midi, Module, Plugin, Setup, State, TimedMidi, Transport};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Dispatch window messages for `ms` milliseconds (JUCE timers and async updates run from here).
fn pump(ms: u64) {
    let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < end {
        #[cfg(windows)]
        // SAFETY: a standard message loop on this thread, which owns the plugin.
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, mode) = match &args[..] {
        [p, m, ..] => (p, m.as_str()),
        _ => anyhow::bail!("usage: vendor PLUGIN.vst3 list | load PRESET [PROGRAM] [PARAM...] | raw STATE.bin [PARAM...]"),
    };
    let module = Arc::new(Module::load(Path::new(path))?);
    let mut plugin = Plugin::new(module, None, Setup { sample_rate: 48000.0, max_block: 512, offline: true })?;
    let class = plugin.class().clone();
    let family = vendor::family(&class.name, &class.vendor);
    println!("{} ({}): {family:?}", class.name, class.vendor);
    let params = plugin.params();
    let before: Vec<f64> = params.iter().map(|p| plugin.param(p.id).unwrap_or(0.0)).collect();
    let mut rest = 3;
    let bytes = match mode {
        "list" => {
            let family = family.ok_or_else(|| anyhow::anyhow!("no vendor preset format for this plugin"))?;
            for (i, p) in vendor::list(family, &class.name).iter().enumerate() {
                println!("  #{i} [{}] {} {:?} {}", p.group, p.name, p.program, p.path.display());
            }
            return Ok(());
        }
        "raw" => std::fs::read(args.get(2).ok_or_else(|| anyhow::anyhow!("raw needs a file"))?)?,
        "load" => {
            let family = family.ok_or_else(|| anyhow::anyhow!("no vendor preset format for this plugin"))?;
            let which = args.get(2).ok_or_else(|| anyhow::anyhow!("load needs a preset"))?;
            let preset = if let Some(n) = which.strip_prefix('#') {
                vendor::list(family, &class.name).into_iter().nth(n.parse()?).ok_or_else(|| anyhow::anyhow!("no preset {which}"))?
            } else {
                let program = args.get(3).and_then(|s| s.parse::<usize>().ok());
                if program.is_some() {
                    rest = 4;
                }
                let path = PathBuf::from(which);
                let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                VendorPreset { name, group: String::new(), path, program }
            };
            println!("preset: {} [{}] program {:?}", preset.name, preset.group, preset.program);
            let template = plugin.state()?.component;
            let st = vendor::to_state(family, &template, &preset)?;
            std::fs::write(std::env::temp_dir().join("flux-vendor-last.bin"), &st)?;
            st
        }
        m => anyhow::bail!("unknown mode {m}"),
    };
    let r = plugin.set_state(&State { component: bytes.clone(), controller: vec![] });
    println!("set_state({} bytes): {r:?}", bytes.len());
    pump(1500);
    let named: Vec<&String> = args.iter().skip(rest).collect();
    let mut changed = 0;
    for (p, b) in params.iter().zip(&before) {
        let v = plugin.param(p.id).unwrap_or(0.0);
        let show = if named.is_empty() { (v - b).abs() > 1e-6 } else { named.iter().any(|n| p.title.eq_ignore_ascii_case(n)) };
        if show {
            changed += 1;
            if changed <= 40 {
                println!("  '{}' {b:.4} -> {v:.4} ({:?})", p.title, plugin.param_text(p.id, v));
            }
        }
    }
    if named.is_empty() {
        println!("{changed} parameters changed");
    }
    let info = plugin.info();
    let mut proc = plugin.processor()?;
    let sr = 48000usize;
    let in_channels = info.inputs.iter().find(|b| b.main).map_or(0, |b| b.channels);
    let input: Vec<Vec<f32>> =
        (0..in_channels).map(|c| (0..sr * 2).map(|i| if i < sr / 2 { (((i * 7919 + c * 104729) % 1000) as f32 / 1000.0 - 0.5) * 0.6 } else { 0.0 }).collect()).collect();
    let inputs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    let (mut l, mut r) = (vec![0f32; sr * 2], vec![0f32; sr * 2]);
    let midi = [
        TimedMidi { offset: 0, msg: Midi::NoteOn { ch: 0, pitch: 60, vel: 100 } },
        TimedMidi { offset: sr as u32, msg: Midi::NoteOff { ch: 0, pitch: 60, vel: 0 } },
    ];
    let tr = Transport { playing: true, ..Default::default() };
    // a few blocks first, with messages pumped: sample players load their samples after a state change
    for _ in 0..4 {
        let (mut a, mut b) = (vec![0f32; 512], vec![0f32; 512]);
        let silent: Vec<Vec<f32>> = (0..in_channels).map(|_| vec![0f32; 512]).collect();
        let s: Vec<&[f32]> = silent.iter().map(Vec::as_slice).collect();
        proc.process(512, &s, &mut [&mut a, &mut b], &[], &[], Some(&tr));
        pump(250);
    }
    let rep = proc.process(sr * 2, &inputs, &mut [&mut l, &mut r], &midi, &[], Some(&tr));
    let db = |x: &[f32]| 10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64 + 1e-30).log10();
    let peak = l.iter().chain(&r).fold(0f32, |a, v| a.max(v.abs()));
    println!("{rep:?}: first second {:.1} dB, second {:.1} dB, peak {:.1} dBFS", db(&l[..sr]), db(&l[sr..]), 20.0 * (peak as f64 + 1e-30).log10());
    Ok(())
}
