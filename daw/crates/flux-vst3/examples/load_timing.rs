//! How long a plugin takes to become playable after a state restore (sample players load in the background).
//!
//!     cargo run -p flux-vst3 --example load_timing -- PLUGIN.vst3 STATE.vstpreset [PITCH]
//!
//! Runs the plugin in a `flux-plugin-host` (whose UI thread pumps messages, as a DAW's does) and restores, in turn:
//! the preset, the same preset again, the plugin's fresh state, and the preset once more. After each it plays a
//! short PITCH (default 60) every 50 ms of wall-clock time and prints the events the plugin sent until a note sounds
//! (or 10 s pass), so the events that mark "loaded" can be told from the ones every restore sends.

use flux_vst3::client::{self, AudioPort, HostOptions, LoadOptions, PluginHost};
use flux_vst3::preset::Preset;
use flux_vst3::{Midi, Setup, TimedMidi, Transport};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, preset, rest @ ..] = &args[..] else { anyhow::bail!("usage: load_timing PLUGIN.vst3 STATE.vstpreset [PITCH]") };
    let pitch: u8 = rest.first().map_or(Ok(60), |p| p.parse())?;
    let setup = Setup { sample_rate: 48000.0, max_block: 512, offline: true };
    let (host, _, mut port) = client::open(HostOptions::default(), &LoadOptions::instrument(path, setup))?;
    let fresh = host.state()?;
    let bytes = Preset::from_bytes(&std::fs::read(preset)?)?.to_bytes()?;
    for (what, state) in [("preset", &bytes), ("same preset again", &bytes), ("fresh state", &fresh), ("preset after fresh", &bytes)] {
        println!("== {what}");
        restore(&host, &mut port, state, pitch)?;
    }
    host.shutdown()?;
    Ok(())
}

fn restore(host: &PluginHost, port: &mut AudioPort, state: &[u8], pitch: u8) -> anyhow::Result<()> {
    let _ = host.take_events();
    let t0 = Instant::now();
    host.set_state(state)?;
    let tr = Transport { playing: true, ..Default::default() };
    let (mut l, mut r) = (vec![0f32; 512], vec![0f32; 512]);
    let midi = [
        TimedMidi { offset: 0, msg: Midi::NoteOn { ch: 0, pitch, vel: 100 } },
        TimedMidi { offset: 500, msg: Midi::NoteOff { ch: 0, pitch, vel: 0 } },
    ];
    while t0.elapsed() < Duration::from_secs(10) {
        port.process(512, &[], &mut [&mut l, &mut r], &midi, &[], Some(&tr))?;
        let peak = l.iter().chain(&r).fold(0f32, |a, v| a.max(v.abs()));
        let evs: Vec<String> = host.take_events().iter().map(|e| format!("{e:?}").chars().take(40).collect()).collect();
        if !evs.is_empty() || peak > 1e-6 {
            println!("{:6} ms  peak {:7.1} dBFS  {} events: {}", t0.elapsed().as_millis(), 20.0 * (peak as f64 + 1e-30).log10(), evs.len(), evs.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
        }
        if peak > 1e-6 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // let the note ring out before the next restore
    for _ in 0..400 {
        port.process(512, &[], &mut [&mut l, &mut r], &[], &[], Some(&tr))?;
    }
    host.reset()?;
    Ok(())
}
