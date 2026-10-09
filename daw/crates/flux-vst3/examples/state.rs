//! Dump a plugin's component state, or load one and hear what it does.
//!
//!     cargo run -p flux-vst3 --example state -- PLUGIN.vst3 dump OUT.bin
//!     cargo run -p flux-vst3 --example state -- PLUGIN.vst3 load IN.bin [PARAM...]
//!
//! `load` sets the bytes as the component state, prints the named parameters (all changed ones when none
//! are named), then plays C4 for one second (an effect gets a noise burst) and reports the level.

use flux_vst3::{Midi, Module, Plugin, Setup, State, TimedMidi, Transport};
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, mode, file) = match &args[..] {
        [p, m, f, ..] => (p, m.as_str(), f),
        _ => anyhow::bail!("usage: state PLUGIN.vst3 dump|load FILE [PARAM...]"),
    };
    let module = Arc::new(Module::load(std::path::Path::new(path))?);
    let mut plugin = Plugin::new(module, None, Setup { sample_rate: 48000.0, max_block: 512, offline: true })?;
    let params = plugin.params();
    let before: Vec<f64> = params.iter().map(|p| plugin.param(p.id).unwrap_or(0.0)).collect();
    match mode {
        "dump" => {
            let st = plugin.state()?;
            std::fs::write(file, &st.component)?;
            println!("component state: {} bytes -> {file}; controller state: {} bytes", st.component.len(), st.controller.len());
            return Ok(());
        }
        "load" => {
            let bytes = std::fs::read(file)?;
            let r = plugin.set_state(&State { component: bytes.clone(), controller: vec![] });
            println!("set_state({} bytes): {r:?}", bytes.len());
        }
        m => anyhow::bail!("unknown mode {m}"),
    }
    let named: Vec<&String> = args[3..].iter().collect();
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
    // a few blocks first: sample players load their samples after a state change
    for _ in 0..4 {
        let (mut a, mut b) = (vec![0f32; 512], vec![0f32; 512]);
        let silent: Vec<Vec<f32>> = (0..in_channels).map(|_| vec![0f32; 512]).collect();
        let s: Vec<&[f32]> = silent.iter().map(Vec::as_slice).collect();
        proc.process(512, &s, &mut [&mut a, &mut b], &[], &[], Some(&tr));
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let rep = proc.process(sr * 2, &inputs, &mut [&mut l, &mut r], &midi, &[], Some(&tr));
    let db = |x: &[f32]| 10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64 + 1e-30).log10();
    let peak = l.iter().chain(&r).fold(0f32, |a, v| a.max(v.abs()));
    println!("{rep:?}: first second {:.1} dB, second {:.1} dB, peak {:.1} dBFS", db(&l[..sr]), db(&l[sr..]), 20.0 * (peak as f64 + 1e-30).log10());
    Ok(())
}
