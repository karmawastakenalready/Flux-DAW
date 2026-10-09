//! The protocol between Flux (client) and `flux-plugin-host` (one process per plugin instance).
//!
//! Control: framed messages over the child's stdin (requests) and a private copy of its stdout
//! (replies and events). A frame is `u32 json_len, u32 blob_len, json, blob`; the blob carries
//! binary payloads (plugin state) so they aren't inflated by JSON.
//!
//! Audio: a shared-memory block ([`ShmHeader`] followed by event, parameter and sample arrays) and
//! two auto-reset events. The client writes a block and signals `request`; the host processes it,
//! writes the output and signals `done`. Each block carries a sequence number so a late reply
//! (after a client timeout) is never mistaken for the current one.

use crate::plugin::{Midi, ParamPoint, ProcessReport, Setup, TimedMidi, Transport};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::atomic::AtomicU64;

/// Bumped on any incompatible change to frames, messages or the shared-memory layout.
pub const PROTOCOL_VERSION: u32 = 2;
/// Largest frame accepted (state blobs of big sample-based instruments can be tens of MB).
pub const MAX_FRAME: usize = 512 << 20;

/// Client -> host.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello { version: u32 },
    /// Load the plugin and attach the audio channel. One per host process.
    Load { path: String, class: Option<String>, setup: Setup, audio: AudioNames, in_channels: u32, out_channels: u32 },
    Info,
    /// Every parameter with its current value and display text (filter: substring of the title;
    /// choices: also the display text of every step of stepped parameters with up to 128 steps).
    Params {
        filter: Option<String>,
        #[serde(default)]
        choices: bool,
    },
    /// Set parameters by name/`#id` with human values (see `params`); returns what each resolved to.
    SetParams { values: Vec<(String, Value)> },
    /// Resolve parameters as `SetParams` would, without setting them: each one's normalized value, and its
    /// value now. One that doesn't resolve has an error of its own instead of failing the rest.
    ResolveParams { values: Vec<(String, Value)> },
    /// The state as a .vstpreset (in the blob).
    GetState,
    /// Restore a .vstpreset (in the blob).
    SetState,
    Programs,
    SetProgram { list: i32, index: usize },
    /// Open the plugin's editor window (optionally owned by `parent`, a native window handle of the DAW).
    OpenEditor { title: String, parent: Option<i64> },
    CloseEditor,
    /// Deactivate and reactivate (clears voices, tails and buffers).
    Reset,
    Shutdown,
}

/// Names of the shared-memory section and the two events.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AudioNames {
    pub shm: String,
    pub request: String,
    pub done: String,
    pub max_events: u32,
    pub max_params: u32,
}

/// Host -> client.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    Reply { id: u64, ok: bool, #[serde(default)] error: Option<String>, #[serde(default)] result: Value },
    Event(HostEvent),
}

/// Things that happen in the host without being asked.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HostEvent {
    /// A parameter was changed in the plugin's UI.
    ParamEdit { id: u32, value: f64 },
    /// The plugin's state changed since it was last saved (edits in its UI, a program change...).
    Dirty,
    /// The plugin's latency changed.
    Latency { samples: u32 },
    /// Parameter list or titles changed (re-read params).
    ParamsChanged,
    EditorOpened,
    EditorClosed,
    Log { message: String },
}

/// A request with its id, as sent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    #[serde(flatten)]
    pub req: Request,
}

pub fn write_frame(w: &mut impl Write, json: &impl Serialize, blob: &[u8]) -> Result<()> {
    let j = serde_json::to_vec(json)?;
    if j.len() > MAX_FRAME || blob.len() > MAX_FRAME {
        bail!("frame too large ({} + {} bytes)", j.len(), blob.len());
    }
    let mut head = [0u8; 8];
    head[..4].copy_from_slice(&(j.len() as u32).to_le_bytes());
    head[4..].copy_from_slice(&(blob.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(&j)?;
    w.write_all(blob)?;
    w.flush()?;
    Ok(())
}

/// Read one frame; Ok(None) at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> Result<Option<(Value, Vec<u8>)>> {
    let mut head = [0u8; 8];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let jl = u32::from_le_bytes(head[..4].try_into()?) as usize;
    let bl = u32::from_le_bytes(head[4..].try_into()?) as usize;
    if jl > MAX_FRAME || bl > MAX_FRAME {
        bail!("frame too large ({jl} + {bl} bytes): the stream is corrupt");
    }
    let mut j = vec![0u8; jl];
    r.read_exact(&mut j).context("reading frame")?;
    let mut b = vec![0u8; bl];
    r.read_exact(&mut b).context("reading frame payload")?;
    let v = serde_json::from_slice(&j).context("frame is not JSON")?;
    Ok(Some((v, b)))
}

// ---- shared memory layout -----------------------------------------------------------------------

pub const SHM_MAGIC: u32 = 0x4254_4E33; // "BTN3"

/// Transport as stored in shared memory.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmTransport {
    pub valid: u32,
    pub playing: u32,
    pub ts_num: i32,
    pub ts_den: i32,
    pub tempo: f64,
    pub project_samples: i64,
    pub ppq: f64,
    pub bar_ppq: f64,
    pub cycle: u32,
    pub _pad: u32,
    pub cycle_start: f64,
    pub cycle_end: f64,
}

impl ShmTransport {
    pub fn from(t: Option<&Transport>) -> ShmTransport {
        match t {
            None => ShmTransport::default(),
            Some(t) => ShmTransport {
                valid: 1,
                playing: t.playing as u32,
                ts_num: t.ts_num,
                ts_den: t.ts_den,
                tempo: t.tempo,
                project_samples: t.project_samples,
                ppq: t.ppq,
                bar_ppq: t.bar_ppq,
                cycle: t.cycle.is_some() as u32,
                _pad: 0,
                cycle_start: t.cycle.map_or(0.0, |c| c.0),
                cycle_end: t.cycle.map_or(0.0, |c| c.1),
            },
        }
    }

    pub fn to(&self) -> Option<Transport> {
        (self.valid != 0).then(|| Transport {
            playing: self.playing != 0,
            tempo: self.tempo,
            ts_num: self.ts_num,
            ts_den: self.ts_den,
            project_samples: self.project_samples,
            ppq: self.ppq,
            bar_ppq: self.bar_ppq,
            cycle: (self.cycle != 0).then_some((self.cycle_start, self.cycle_end)),
        })
    }
}

/// One MIDI event in shared memory: kind 1 note on, 2 note off, 3 cc, 4 pitch bend, 5 channel pressure.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmMidi {
    pub offset: u32,
    pub kind: u8,
    pub ch: u8,
    pub a: u8,
    pub b: u8,
    pub value: i32,
}

impl ShmMidi {
    pub fn from(e: &TimedMidi) -> ShmMidi {
        let (kind, ch, a, b, value) = match e.msg {
            Midi::NoteOn { ch, pitch, vel } => (1, ch, pitch, vel, 0),
            Midi::NoteOff { ch, pitch, vel } => (2, ch, pitch, vel, 0),
            Midi::Cc { ch, cc, value } => (3, ch, cc, value, 0),
            Midi::PitchBend { ch, value } => (4, ch, 0, 0, value as i32),
            Midi::ChannelPressure { ch, value } => (5, ch, value, 0, 0),
        };
        ShmMidi { offset: e.offset, kind, ch, a, b, value }
    }

    pub fn to(&self) -> Option<TimedMidi> {
        let (ch, a, b) = (self.ch & 15, self.a & 127, self.b & 127);
        let msg = match self.kind {
            1 => Midi::NoteOn { ch, pitch: a, vel: b },
            2 => Midi::NoteOff { ch, pitch: a, vel: b },
            3 => Midi::Cc { ch, cc: a, value: b },
            4 => Midi::PitchBend { ch, value: self.value.clamp(-8192, 8191) as i16 },
            5 => Midi::ChannelPressure { ch, value: a },
            _ => return None,
        };
        Some(TimedMidi { offset: self.offset, msg })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmParam {
    pub id: u32,
    pub offset: u32,
    pub value: f64,
}

impl ShmParam {
    pub fn from(p: &ParamPoint) -> ShmParam {
        ShmParam { id: p.id, offset: p.offset, value: p.value }
    }
    pub fn to(&self) -> ParamPoint {
        ParamPoint { id: self.id, offset: self.offset, value: self.value }
    }
}

/// Start of the shared block. Request fields are written by the client before it signals `request`;
/// response fields by the host before it signals `done`. The event signal/wait pair orders the memory.
#[repr(C)]
pub struct ShmHeader {
    pub magic: u32,
    pub version: u32,
    pub max_block: u32,
    pub in_channels: u32,
    pub out_channels: u32,
    pub max_events: u32,
    pub max_params: u32,
    pub _pad0: u32,
    // request
    pub seq: AtomicU64,
    pub frames: u32,
    pub n_events: u32,
    pub n_params: u32,
    /// Bit 0: inputs carry audio.
    pub flags: u32,
    pub transport: ShmTransport,
    // response
    pub done_seq: AtomicU64,
    pub status: i32,
    pub latency: u32,
    pub skipped: u32,
    pub unmapped: u32,
    pub dropped: u32,
    pub _pad1: u32,
}

/// Byte offsets of each region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShmLayout {
    pub events: usize,
    pub params: usize,
    pub input: usize,
    pub output: usize,
    pub size: usize,
}

fn align(x: usize) -> usize {
    (x + 63) & !63
}

impl ShmLayout {
    pub fn new(max_block: usize, in_ch: usize, out_ch: usize, max_events: usize, max_params: usize) -> ShmLayout {
        let events = align(std::mem::size_of::<ShmHeader>());
        let params = align(events + max_events * std::mem::size_of::<ShmMidi>());
        let input = align(params + max_params * std::mem::size_of::<ShmParam>());
        let output = align(input + in_ch * max_block * 4);
        let size = align(output + out_ch * max_block * 4);
        ShmLayout { events, params, input, output, size }
    }
}

impl ProcessReport {
    pub fn from_header(h: &ShmHeader) -> ProcessReport {
        ProcessReport { skipped: h.skipped != 0, unmapped: h.unmapped, dropped: h.dropped }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = vec![];
        let env = Envelope { id: 7, req: Request::SetProgram { list: 3, index: 4 } };
        write_frame(&mut buf, &env, b"blob").unwrap();
        write_frame(&mut buf, &Message::Event(HostEvent::Latency { samples: 64 }), &[]).unwrap();
        let mut r = &buf[..];
        let (v, b) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(b, b"blob");
        let e: Envelope = serde_json::from_value(v).unwrap();
        assert_eq!(e.id, 7);
        assert!(matches!(e.req, Request::SetProgram { list: 3, index: 4 }));
        let (v, _) = read_frame(&mut r).unwrap().unwrap();
        let m: Message = serde_json::from_value(v).unwrap();
        assert!(matches!(m, Message::Event(HostEvent::Latency { samples: 64 })));
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn corrupt_frames_are_rejected() {
        let mut r: &[u8] = &[0xff, 0xff, 0xff, 0x7f, 0, 0, 0, 0];
        assert!(read_frame(&mut r).is_err());
        let mut r: &[u8] = &[4, 0, 0, 0, 0, 0, 0, 0, b'{'];
        assert!(read_frame(&mut r).is_err());
    }

    #[test]
    fn midi_and_transport_round_trip() {
        for m in [
            Midi::NoteOn { ch: 3, pitch: 60, vel: 100 },
            Midi::NoteOff { ch: 0, pitch: 61, vel: 0 },
            Midi::Cc { ch: 15, cc: 1, value: 127 },
            Midi::PitchBend { ch: 1, value: -8192 },
            Midi::ChannelPressure { ch: 2, value: 33 },
        ] {
            let e = TimedMidi { offset: 99, msg: m };
            assert_eq!(ShmMidi::from(&e).to(), Some(e));
        }
        let t = Transport { playing: true, tempo: 133.0, ts_num: 6, ts_den: 8, project_samples: 12345, ppq: 7.5, bar_ppq: 6.0, cycle: Some((0.0, 16.0)) };
        assert_eq!(ShmTransport::from(Some(&t)).to(), Some(t));
        assert_eq!(ShmTransport::from(None).to(), None);
    }

    #[test]
    fn layout_is_aligned_and_sized() {
        let l = ShmLayout::new(512, 2, 2, 2048, 512);
        assert_eq!(l.events % 64, 0);
        assert!(l.output >= l.input + 2 * 512 * 4);
        assert!(l.size >= l.output + 2 * 512 * 4);
    }
}
