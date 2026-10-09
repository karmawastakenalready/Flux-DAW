//! Load a plugin in this process, print what it is, play a note and report the level.
//!
//!     cargo run -p flux-vst3 --example probe -- "plugins/dexed/Dexed.vst3"

use flux_vst3::{Midi, Module, Plugin, Setup, TimedMidi, Transport};
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: probe <plugin.vst3>");
    let t0 = std::time::Instant::now();
    let module = Arc::new(Module::load(std::path::Path::new(&path))?);
    println!("loaded in {:?}: {:?}", t0.elapsed(), module.paths);
    println!("factory: {:?}", module.factory_info());
    for c in module.classes()? {
        println!("  class {} '{}' [{}] {} v{} sdk {}", c.cid, c.name, c.category, c.sub_categories, c.version, c.sdk_version);
    }
    let t1 = std::time::Instant::now();
    let mut plugin = Plugin::new(module.clone(), None, Setup { sample_rate: 48000.0, max_block: 512, offline: true })?;
    println!("instantiated in {:?}", t1.elapsed());
    let info = plugin.info();
    println!("info: {}", serde_json::to_string(&info)?);
    let params = plugin.params();
    println!("{} params; first 8:", params.len());
    for p in params.iter().take(8) {
        let v = plugin.param(p.id).unwrap_or(0.0);
        println!("  {} '{}' = {v:.3} ({:?}) steps {} flags {:#x}", p.id, p.title, plugin.param_text(p.id, v), p.step_count, p.flags);
    }
    for l in plugin.program_lists() {
        println!("program list {} '{}' unit {} param {:?}: {} programs, first {:?}", l.id, l.name, l.unit_id, l.param, l.programs.len(), l.programs.iter().take(5).collect::<Vec<_>>());
    }
    println!("midi map: {} controllers mapped on ch 1", plugin.midi_map().mapped());
    if let Some(json) = std::env::args().nth(2) {
        let want: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&json)?;
        for (k, v) in &want {
            match flux_vst3::params::find(&params, k).and_then(|p| flux_vst3::params::normalized(&plugin, p, v).map(|n| (p, n))) {
                Ok((p, n)) => {
                    plugin.set_param(p.id, n);
                    println!("  {k} = {v} -> '{}' #{} normalized {n:.6} shows {:?}", p.title, p.id, plugin.param_text(p.id, n));
                }
                Err(e) => println!("  {k} = {v}: ERROR {e:#}"),
            }
        }
    }
    let mut proc = plugin.processor()?;
    let sr = 48000usize;
    let mut l = vec![0f32; sr * 2];
    let mut r = vec![0f32; sr * 2];
    let midi = [
        TimedMidi { offset: 0, msg: Midi::NoteOn { ch: 0, pitch: 60, vel: 100 } },
        TimedMidi { offset: sr as u32, msg: Midi::NoteOff { ch: 0, pitch: 60, vel: 0 } },
    ];
    // effects get a test burst on their main input: 0.5 s of noise at about -15 dB RMS, then silence (the tail)
    let in_channels = info.inputs.iter().find(|b| b.main).map_or(0, |b| b.channels);
    let mut seed = 0x2545_f491_u32;
    let input: Vec<Vec<f32>> = (0..in_channels)
        .map(|_| {
            (0..sr * 2)
                .map(|i| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    if i < sr / 2 { (seed as f32 / u32::MAX as f32 - 0.5) * 0.6 } else { 0.0 }
                })
                .collect()
        })
        .collect();
    let inputs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    let t2 = std::time::Instant::now();
    let tr = Transport { playing: true, ..Default::default() };
    let rep = proc.process(sr * 2, &inputs, &mut [&mut l, &mut r], &midi, &[], Some(&tr));
    if let Some(x) = input.first() {
        let rms = (x[..sr / 2].iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / (sr / 2) as f64).sqrt();
        println!("fed {in_channels} input channel(s): 0.5 s burst at {:.1} dB RMS, then silence", 20.0 * rms.log10());
    }
    println!("rendered 2 s in {:?}: {rep:?}", t2.elapsed());
    let rms = |x: &[f32]| (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
    let peak = l.iter().chain(&r).fold(0f32, |a, v| a.max(v.abs()));
    println!("rms L {:.1} dB, R {:.1} dB, peak {:.1} dB; second half {:.1} dB", 20.0 * rms(&l[..sr]).log10(), 20.0 * rms(&r[..sr]).log10(), 20.0 * (peak as f64).log10(), 20.0 * rms(&l[sr..]).log10());
    drop(proc);
    let st = plugin.state()?;
    println!("state: component {} bytes, controller {} bytes", st.component.len(), st.controller.len());
    plugin.set_state(&st)?;
    let st2 = plugin.state()?;
    println!("state round trip identical: {}", st2 == st);
    drop(plugin);
    println!("unloaded ok");
    Ok(())
}
