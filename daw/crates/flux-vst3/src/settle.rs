//! Waiting for a state restore that finishes in the background.
//!
//! Sample players such as Decent Sampler read a preset at once but load its samples later on their message
//! thread, and drop every note that arrives before the samples are in: a render started straight after the
//! restore comes out silent. There's no VST3 call that says "loaded", so [`wait_playable`] watches for both
//! signs of it: the burst of control edits the plugin sends when it finishes (its knobs set from the new preset),
//! and a short probe note that sounds. Either can be missing (a cached library sends no burst; a probe can fall
//! on an unmapped key), which is why it takes both.
//!
//! A sample player's round robins carry on from note to note, and only a restore puts them back to the start: so
//! that a render comes out the same every time, [`wait_playable`] always ends the same way, with one probe note
//! played on the loaded samples (dropped probes leave no trace).

use crate::client::{AudioPort, PluginHost};
use crate::ipc::HostEvent;
use crate::plugin::{Midi, TimedMidi, Transport};
use anyhow::{anyhow, Result};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// How a background restore ended, and how long it took.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Settled {
    /// The plugin sent its end-of-loading edits.
    Signalled(Duration),
    /// A probe note sounded.
    Sounded(Duration),
    /// Neither happened in the time allowed (the start of a render may be silent).
    TimedOut(Duration),
}

impl Settled {
    pub fn timed_out(&self) -> bool {
        matches!(self, Settled::TimedOut(_))
    }
}

/// Output below this is silence (-120 dBFS).
const SILENT: f32 = 1e-6;
/// Between polls.
const POLL: Duration = Duration::from_millis(20);
/// Ringing out stops once the output has been silent this long (seconds of audio)...
const QUIET_S: f64 = 1.0;
/// ...or after this much audio, whatever it does.
const MAX_RING_S: f64 = 60.0;
/// After the plugin signals, how long a probe has to sound before Flux gives up on it (an unmapped key).
const AFTER_SIGNAL: Duration = Duration::from_secs(2);

/// Wait until a plugin can play after a state restore: until it signals that it finished loading (`watch` must
/// have been opened with [`PluginHost::watch`] before the restore) or a short `probe` note (pitch, velocity)
/// sounds, polling for at most `timeout`. After a signal it still probes until one note sounds (for a moment), so
/// the round robins end up in the same place however the wait ended. Then lets the probe ring out and resets the
/// plugin, so the next block starts from silence.
pub fn wait_playable(host: &PluginHost, port: &mut AudioPort, watch: &Receiver<HostEvent>, probe: Option<(u8, u8)>, timeout: Duration) -> Result<Settled> {
    let t0 = Instant::now();
    let block = port.setup().max_block.clamp(1, 512);
    let inputs = vec![vec![0f32; block]; port.in_channels()];
    let inputs: Vec<&[f32]> = inputs.iter().map(Vec::as_slice).collect();
    let mut out = vec![vec![0f32; block]; port.out_channels().max(1)];
    let transport = Transport::default();
    let mut probed = false;
    let mut signal = None;
    let settled = loop {
        if signal.is_none() && signalled(watch)? {
            signal = Some(t0.elapsed());
        }
        match (signal, probe) {
            (Some(at), None) => break Settled::Signalled(at),
            (Some(at), Some(_)) if t0.elapsed() >= at + AFTER_SIGNAL => break Settled::Signalled(at),
            _ => {}
        }
        if let Some((pitch, vel)) = probe {
            let midi = [
                TimedMidi { offset: 0, msg: Midi::NoteOn { ch: 0, pitch, vel } },
                TimedMidi { offset: (block / 2) as u32, msg: Midi::NoteOff { ch: 0, pitch, vel: 0 } },
            ];
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            port.process(block, &inputs, &mut outs, &midi, &[], Some(&transport)).map_err(|e| anyhow!("{e}"))?;
            probed = true;
            if peak(&out) > SILENT {
                break signal.map_or(Settled::Sounded(t0.elapsed()), Settled::Signalled);
            }
        }
        if signal.is_none() && t0.elapsed() >= timeout {
            break Settled::TimedOut(t0.elapsed());
        }
        std::thread::sleep(POLL);
    };
    if probed {
        ring_out(port, &inputs, &mut out)?;
    }
    host.reset()?;
    Ok(settled)
}

/// Whether the plugin sent the edits that end its loading (and the host is still there).
fn signalled(watch: &Receiver<HostEvent>) -> Result<bool> {
    loop {
        match watch.try_recv() {
            Ok(HostEvent::ParamEdit { .. } | HostEvent::Dirty) => return Ok(true),
            Ok(_) => {}
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => return Err(anyhow!("the plugin host stopped while the plugin was loading")),
        }
    }
}

/// Process silence until the output stays silent for `QUIET_S` (the probes' release and any built-in reverb).
fn ring_out(port: &mut AudioPort, inputs: &[&[f32]], out: &mut [Vec<f32>]) -> Result<()> {
    let block = out[0].len();
    let sr = port.setup().sample_rate;
    let (quiet_blocks, max_blocks) = ((QUIET_S * sr / block as f64).ceil() as usize, (MAX_RING_S * sr / block as f64) as usize);
    let mut quiet = 0;
    for _ in 0..max_blocks {
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        port.process(block, inputs, &mut outs, &[], &[], None).map_err(|e| anyhow!("{e}"))?;
        quiet = if peak(out) > SILENT { 0 } else { quiet + 1 };
        if quiet >= quiet_blocks {
            break;
        }
    }
    Ok(())
}

fn peak(out: &[Vec<f32>]) -> f32 {
    out.iter().flatten().fold(0f32, |m, v| m.max(v.abs()))
}
