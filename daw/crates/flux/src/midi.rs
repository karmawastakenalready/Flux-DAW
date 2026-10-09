//! Playing in: MIDI keyboards, the computer keyboard and the piano roll's keys play the target track's instrument
//! (the track selected in the GUI) through the live engine, with the song or on their own, and record onto it while
//! the song plays live.
//!
//! Every MIDI input is opened and listened to on all channels unless it is turned off (Transport menu; remembered
//! in `%LOCALAPPDATA%/Flux/midi.json`). A key's release goes where its press went, and a sustain pedal's release to
//! every track it held, so changing tracks mid-phrase leaves nothing hanging. A take records each event at the song
//! position being heard when it was played, and becomes notes, controllers and bends on its track: one undo step for
//! each pass of a loop, and one at the end.

use crate::audio::Mode;
use crate::state::Shared;
use eframe::egui;
use flux_core::commands::{BendSpec, CcSpec, Command, NoteSpec};
use flux_core::pitch::PitchSpec;
use flux_core::{Project, Source, TimeMap, TrackRef};
use flux_engine::sfizz::Ev;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Where events come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// A MIDI device (its id while it stays plugged in).
    Device(u32),
    /// The computer keyboard played as a piano.
    Keys,
    /// The piano roll's keyboard, clicked.
    Mouse,
}

/// A MIDI input, for the Transport menu.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    /// Not turned off.
    pub on: bool,
    pub connected: bool,
    /// Why it couldn't be opened (another program may have it).
    pub error: Option<String>,
}

/// A recording in progress, for drawing: what has been played but isn't on the track yet, in song beats.
#[derive(Clone, Debug)]
pub struct TakeView {
    pub track: u64,
    /// Where recording started.
    pub from: f64,
    /// It has gone round the whole loop (it covers the whole song).
    pub around: bool,
    /// (start, length, pitch, velocity); notes still held run to the position being heard.
    pub notes: Vec<(f64, f64, u8, u8)>,
}

struct Device {
    id: u32,
    name: String,
    on: bool,
    conn: Option<midir::MidiInputConnection<()>>,
    error: Option<String>,
}

#[derive(Default)]
struct Router {
    /// The track notes go to.
    target: Option<u64>,
    /// Keys down: where from, the pitch, the track the press went to.
    held: Vec<(Input, u8, u64)>,
    /// Sustain pedals down: where from, the track.
    pedals: Vec<(Input, u64)>,
}

impl Router {
    /// Where an event goes: usually the target track, but a release goes where its press went (and a pedal or
    /// all-notes-off to every track it holds notes on).
    fn route(&mut self, from: Input, ev: Ev) -> Vec<(u64, Ev)> {
        let mut to: Vec<u64> = vec![];
        match ev {
            Ev::On(p, _) => {
                if let Some(t) = self.target {
                    self.held.push((from, p, t));
                    to.push(t);
                }
            }
            Ev::Off(p) => match self.held.iter().position(|h| h.0 == from && h.1 == p) {
                Some(k) => to.push(self.held.swap_remove(k).2),
                None => to.extend(self.target),
            },
            Ev::Cc(64, v) if v >= 64 => {
                if let Some(t) = self.target {
                    if !self.pedals.contains(&(from, t)) {
                        self.pedals.push((from, t));
                    }
                    to.push(t);
                }
            }
            Ev::Cc(64, _) => {
                to.extend(self.pedals.iter().filter(|x| x.0 == from).map(|x| x.1));
                self.pedals.retain(|x| x.0 != from);
                to.extend(self.target);
            }
            Ev::Cc(120 | 123, _) => {
                to.extend(self.held.iter().filter(|h| h.0 == from).map(|h| h.2));
                self.held.retain(|h| h.0 != from);
                to.extend(self.target);
            }
            _ => to.extend(self.target),
        }
        let mut seen = vec![];
        to.retain(|t| {
            let first = !seen.contains(t);
            seen.push(*t);
            first
        });
        to.into_iter().map(|t| (t, ev)).collect()
    }

    /// Releases for everything `from` holds down (keys and pedal).
    fn releases(&self, from: Input) -> Vec<Ev> {
        let mut evs: Vec<Ev> = self.held.iter().filter(|h| h.0 == from).map(|h| Ev::Off(h.1)).collect();
        if self.pedals.iter().any(|x| x.0 == from) {
            evs.push(Ev::Cc(64, 0));
        }
        evs
    }
}

#[derive(Default)]
struct Inner {
    router: Router,
    devices: Vec<Device>,
    next_id: u32,
    /// Device names turned off (remembered).
    off: Vec<String>,
    /// The computer keyboard plays notes (remembered).
    keys: bool,
    last_event: Option<Instant>,
    take: Option<Take>,
}

pub struct Midi {
    inner: Mutex<Inner>,
    /// Repaints the GUI when something is played.
    ctx: Mutex<Option<egui::Context>>,
    /// The device thread (woken when an input is turned on or off).
    thread: Mutex<Option<std::thread::Thread>>,
    /// What would have gone to the live engine (tests).
    #[cfg(test)]
    pub sent: Mutex<Vec<(u64, Ev)>>,
    /// The song position being heard, in place of the player's (tests).
    #[cfg(test)]
    pub clock: Mutex<Option<f64>>,
}

/// Recorded controller and bend points closer together than this (s) are merged (a mod wheel sends hundreds a
/// second).
const THIN: f64 = 0.01;
/// The shortest recorded note (beats).
const MIN_DUR: f64 = 1.0 / 64.0;

impl Default for Midi {
    fn default() -> Self {
        Midi::new()
    }
}

impl Midi {
    pub fn new() -> Midi {
        Midi {
            inner: Mutex::new(Inner::default()),
            ctx: Mutex::new(None),
            thread: Mutex::new(None),
            #[cfg(test)]
            sent: Mutex::new(vec![]),
            #[cfg(test)]
            clock: Mutex::new(None),
        }
    }

    /// Start listening: open the MIDI inputs (and keep up with devices plugged in and out) and remember the GUI
    /// to repaint.
    pub fn start(sh: &Arc<Shared>, ctx: egui::Context) {
        let m = &sh.midi;
        *m.ctx.lock() = Some(ctx);
        let s = Settings::load();
        let keys = s.computer_keyboard;
        {
            let mut i = m.inner.lock();
            i.off = s.off;
            i.keys = keys;
        }
        if keys {
            m.ready(sh);
        }
        let weak = Arc::downgrade(sh);
        if let Ok(h) = std::thread::Builder::new().name("midi".into()).spawn(move || device_thread(weak)) {
            *m.thread.lock() = Some(h.thread().clone());
        }
    }

    /// The track notes go to: the selected one (None: they go nowhere).
    pub fn set_target(&self, track: Option<u64>) {
        self.inner.lock().router.target = track;
    }

    pub fn target(&self) -> Option<u64> {
        self.inner.lock().router.target
    }

    /// The MIDI inputs found.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.inner
            .lock()
            .devices
            .iter()
            .map(|d| DeviceInfo { name: d.name.clone(), on: d.on, connected: d.conn.is_some(), error: d.error.clone() })
            .collect()
    }

    /// Turn an input on or off (remembered).
    pub fn set_device(&self, name: &str, on: bool) {
        {
            let mut m = self.inner.lock();
            m.off.retain(|n| n != name);
            if !on {
                m.off.push(name.to_string());
            }
            if let Some(d) = m.devices.iter_mut().find(|d| d.name == name) {
                d.on = on;
                d.error = None;
            }
            Settings { off: m.off.clone(), computer_keyboard: m.keys }.save();
        }
        if let Some(t) = &*self.thread.lock() {
            t.unpark();
        }
    }

    /// The computer keyboard plays notes.
    pub fn keys_on(&self) -> bool {
        self.inner.lock().keys
    }

    /// Play notes with the computer keyboard, or stop (what it holds is let go). Remembered.
    pub fn set_keys(&self, sh: &Arc<Shared>, on: bool) {
        {
            let mut m = self.inner.lock();
            m.keys = on;
            Settings { off: m.off.clone(), computer_keyboard: on }.save();
        }
        if on {
            self.ready(sh);
        } else {
            self.release(sh, Input::Keys);
            self.update_quick(sh);
        }
    }

    /// Something can play in: a MIDI input is open or the computer keyboard plays notes.
    pub fn can_play(&self) -> bool {
        let m = self.inner.lock();
        m.keys || m.devices.iter().any(|d| d.conn.is_some())
    }

    /// When something was last played in.
    pub fn last_event(&self) -> Option<Instant> {
        self.inner.lock().last_event
    }

    /// Pitches held down on a track.
    pub fn held(&self, track: u64) -> Vec<u8> {
        self.inner.lock().router.held.iter().filter(|h| h.2 == track).map(|h| h.1).collect()
    }

    /// Play an event from `from`: to its track's instrument, and into the take if it records that track.
    pub fn play(&self, sh: &Arc<Shared>, from: Input, ev: Ev) {
        let sec = self.heard(sh);
        let out = {
            let mut m = self.inner.lock();
            m.last_event = Some(Instant::now());
            let out = m.router.route(from, ev);
            if let (Some(take), Some(s)) = (m.take.as_mut(), sec)
                && out.iter().any(|(t, _)| *t == take.track)
            {
                take.add(from, s, ev);
            }
            out
        };
        for (t, ev) in out {
            self.send(sh, t, ev);
        }
        if let Some(c) = &*self.ctx.lock() {
            c.request_repaint();
        }
    }

    /// Let go of everything `from` holds (a device went away, the window lost focus).
    pub fn release(&self, sh: &Arc<Shared>, from: Input) {
        let evs = self.inner.lock().router.releases(from);
        for ev in evs {
            self.play(sh, from, ev);
        }
    }

    fn send(&self, sh: &Arc<Shared>, track: u64, ev: Ev) {
        #[cfg(test)]
        {
            let _ = sh;
            self.sent.lock().push((track, ev));
        }
        #[cfg(not(test))]
        sh.player.input(track, ev, || sh.lock().session.project.clone());
    }

    /// Something can play in: start the live engine and the output now (so the first note is heard) and keep the
    /// live queue short.
    fn ready(&self, sh: &Arc<Shared>) {
        self.update_quick(sh);
        #[cfg(not(test))]
        sh.player.prepare(|| sh.lock().session.project.clone());
    }

    fn update_quick(&self, sh: &Shared) {
        sh.player.set_quick(self.can_play());
    }

    /// The song second being heard now, while the song plays live.
    fn heard(&self, sh: &Shared) -> Option<f64> {
        #[cfg(test)]
        if let Some(s) = *self.clock.lock() {
            return Some(s);
        }
        sh.player.heard_seconds(Instant::now())
    }

    fn playing_live(&self, sh: &Shared) -> bool {
        #[cfg(test)]
        if self.clock.lock().is_some() {
            return true;
        }
        sh.player.is_playing() && sh.player.mode() == Mode::Live
    }

    // -----------------------------------------------------------------------------------------
    // recording

    /// The track being recorded onto.
    pub fn recording(&self) -> Option<u64> {
        self.inner.lock().take.as_ref().map(|t| t.track)
    }

    /// Start recording onto `track` (the song plays live, or is about to).
    pub fn record(&self, sh: &Shared, track: u64) {
        let take = Take::new(track, &sh.lock().session.project);
        self.inner.lock().take = Some(take);
    }

    /// Stop recording: what was played goes onto the track. Returns what to tell the user.
    pub fn end_take(&self, sh: &Shared) -> Option<String> {
        let part = self.inner.lock().take.take()?.drain(true);
        part.and_then(|p| commit(sh, p))
    }

    /// The song jumped (a seek): keep what was played so far and record on from the new position.
    pub fn cut(&self, sh: &Shared) -> Option<String> {
        let track = self.recording()?;
        let msg = self.end_take(sh);
        self.record(sh, track);
        msg
    }

    /// Called every frame while recording: follows the song, puts each finished pass of a loop on the track, and
    /// ends the take when the song stops. Returns what to tell the user.
    pub fn tick(&self, sh: &Shared) -> Option<String> {
        let live = self.playing_live(sh);
        let sec = self.heard(sh);
        let part = {
            let mut m = self.inner.lock();
            let take = m.take.as_mut()?;
            if !live {
                m.take.take()?.drain(true)
            } else {
                if let Some(s) = sec {
                    take.at(s);
                }
                let pass = take.pass();
                if pass > take.committed {
                    take.committed = pass;
                    take.drain(false)
                } else {
                    None
                }
            }
        };
        part.and_then(|p| commit(sh, p))
    }

    /// The recording in progress, for drawing.
    pub fn take_view(&self) -> Option<TakeView> {
        self.inner.lock().take.as_ref().map(Take::view)
    }
}

/// Put a take (or a pass of one) on its track as one undo step. Returns what to tell the user.
fn commit(sh: &Shared, part: Part) -> Option<String> {
    let name = sh.lock().session.project.tracks.iter().find(|t| t.id == part.track).map(|t| t.name.clone());
    let Some(name) = name else {
        return Some("The track being recorded was removed, so the recording wasn't kept.".into());
    };
    match sh.apply(&part.cmds, Source::Human) {
        Ok(_) if part.notes == 0 => Some(format!("Recorded controllers on {name}")),
        Ok(_) => {
            let (a, b) = part.bars;
            let bars = if b > a { format!("bars {a}\u{2013}{b}") } else { format!("bar {a}") };
            Some(format!("Recorded {} note{} on {name} ({bars})", part.notes, if part.notes == 1 { "" } else { "s" }))
        }
        Err(e) => Some(format!("Couldn't keep the recording: {e:#}")),
    }
}

/// A note, controller or bend message, from any channel (others are ignored).
pub fn parse(msg: &[u8]) -> Option<Ev> {
    let (&status, data) = msg.split_first()?;
    let d = |i: usize| data.get(i).map(|b| b & 0x7F);
    match status & 0xF0 {
        0x90 => match (d(0)?, d(1)?) {
            (p, 0) => Some(Ev::Off(p)),
            (p, v) => Some(Ev::On(p, v)),
        },
        0x80 => Some(Ev::Off(d(0)?)),
        0xB0 => Some(Ev::Cc(d(0)?, d(1)?)),
        0xE0 => Some(Ev::Bend((((d(1)? as i16) << 7) | d(0)? as i16) - 8192)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// takes

/// What a take (or a pass of it) adds to its track.
struct Part {
    track: u64,
    cmds: Vec<Command>,
    notes: usize,
    /// First and last bar with notes.
    bars: (u32, u32),
}

/// A recording: events at the song position heard when they were played, in seconds "unwrapped" (a looping song's
/// second pass carries on from the end of the first), so notes held across the loop point keep their length.
struct Take {
    track: u64,
    tm: TimeMap,
    /// Song length in beats.
    total: f64,
    /// Song length in seconds, when it loops.
    wrap: Option<f64>,
    first: Option<f64>,
    last: Option<f64>,
    /// The pass last put on the track.
    committed: u64,
    /// Keys down: where from, pitch, start, velocity.
    open: Vec<(Input, u8, f64, u8)>,
    /// (start, end, pitch, velocity)
    notes: Vec<(f64, f64, u8, u8)>,
    ccs: Vec<(f64, u8, u8)>,
    bends: Vec<(f64, i16)>,
}

impl Take {
    fn new(track: u64, p: &Project) -> Take {
        let tm = TimeMap::new(p);
        let total = p.total_beats();
        let wrap = p.looped.then(|| tm.sec(total)).filter(|&s| s > 0.0);
        Take { track, tm, total, wrap, first: None, last: None, committed: 0, open: vec![], notes: vec![], ccs: vec![], bends: vec![] }
    }

    /// Unwrap a song second: the time nearest the last one seen (events can arrive a little out of order).
    fn at(&mut self, sec: f64) -> f64 {
        let u = match (self.wrap, self.last) {
            (Some(len), Some(last)) => sec + ((last - sec) / len).round() * len,
            _ => sec,
        };
        self.first.get_or_insert(u);
        self.last = Some(u);
        u
    }

    /// The pass of the loop being played (0 without a loop).
    fn pass(&self) -> u64 {
        match (self.wrap, self.last) {
            (Some(len), Some(u)) => (u / len).floor().max(0.0) as u64,
            _ => 0,
        }
    }

    fn add(&mut self, from: Input, sec: f64, ev: Ev) {
        let u = self.at(sec);
        match ev {
            Ev::On(p, v) => self.open.push((from, p, u, v)),
            Ev::Off(p) => {
                if let Some(k) = self.open.iter().position(|o| o.0 == from && o.1 == p) {
                    let (_, p, s, v) = self.open.remove(k);
                    self.notes.push((s, u, p, v));
                }
            }
            Ev::Cc(120 | 123, _) => {
                let (gone, keep): (Vec<_>, Vec<_>) = self.open.drain(..).partition(|o| o.0 == from);
                self.open = keep;
                self.notes.extend(gone.into_iter().map(|(_, p, s, v)| (s, u, p, v)));
            }
            Ev::Cc(c, v) => match self.ccs.iter_mut().rev().find(|x| x.1 == c) {
                Some(x) if u - x.0 < THIN && u >= x.0 => x.2 = v,
                _ => self.ccs.push((u, c, v)),
            },
            Ev::Bend(b) => match self.bends.last_mut() {
                Some(x) if u - x.0 < THIN && u >= x.0 => x.1 = b,
                _ => self.bends.push((u, b)),
            },
        }
    }

    /// Unwrapped seconds to unwrapped beats.
    fn ubeat(&self, u: f64) -> f64 {
        match self.wrap {
            Some(len) => {
                let k = (u / len).floor();
                k * self.total + self.tm.beat(u - k * len)
            }
            None => self.tm.beat(u.max(0.0)),
        }
    }

    /// Where in the song an unwrapped second falls (beats).
    fn song_beat(&self, u: f64) -> f64 {
        let b = self.ubeat(u);
        tidy(if self.wrap.is_some() { b.rem_euclid(self.total) } else { b.max(0.0) })
    }

    /// A note from `s` to `e` (unwrapped seconds): its start and length in beats. In a loop it stops at the end.
    fn place(&self, s: f64, e: f64) -> (f64, f64) {
        let start = self.song_beat(s);
        let mut dur = (self.ubeat(e) - self.ubeat(s)).max(MIN_DUR);
        if self.wrap.is_some() {
            dur = dur.min(self.total - start).max(MIN_DUR);
        }
        (start, tidy(dur))
    }

    /// What has been played and finished, as commands (and taken out of the take); with `all`, the keys still down
    /// end where the song is now. None when there's nothing.
    fn drain(&mut self, all: bool) -> Option<Part> {
        if all {
            let end = self.last.unwrap_or(0.0);
            let open = std::mem::take(&mut self.open);
            self.notes.extend(open.into_iter().map(|(_, p, s, v)| (s, end.max(s), p, v)));
        }
        if self.notes.is_empty() && self.ccs.is_empty() && self.bends.is_empty() {
            return None;
        }
        let track = TrackRef::Id(self.track);
        let mut cmds = vec![];
        let mut specs = vec![];
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for (s, e, p, v) in std::mem::take(&mut self.notes) {
            let (beat, dur) = self.place(s, e);
            lo = lo.min(beat);
            hi = hi.max(beat + dur);
            specs.push(NoteSpec { bar: None, beat, dur, pitch: PitchSpec::Num(p), vel: v.clamp(1, 127) });
        }
        let notes = specs.len();
        if !specs.is_empty() {
            cmds.push(Command::AddNotes { track: track.clone(), notes: specs });
        }
        let mut by_cc: BTreeMap<u8, Vec<CcSpec>> = BTreeMap::new();
        for (u, c, v) in std::mem::take(&mut self.ccs) {
            by_cc.entry(c).or_default().push(CcSpec { bar: None, beat: self.song_beat(u), value: v });
        }
        for (cc, points) in by_cc {
            cmds.push(Command::AddCc { track: track.clone(), cc, points });
        }
        let bends: Vec<BendSpec> = std::mem::take(&mut self.bends).into_iter().map(|(u, b)| BendSpec { bar: None, beat: self.song_beat(u), value: b }).collect();
        if !bends.is_empty() {
            cmds.push(Command::AddBend { track, points: bends });
        }
        let bars = if notes > 0 { (self.tm.bar_of(lo), self.tm.bar_of((hi - 1e-6).max(lo))) } else { (0, 0) };
        Some(Part { track: self.track, cmds, notes, bars })
    }

    fn view(&self) -> TakeView {
        let now = self.last.unwrap_or(0.0);
        let mut notes = self.notes.clone();
        notes.extend(self.open.iter().map(|&(_, p, s, v)| (s, now.max(s), p, v)));
        let notes = notes
            .into_iter()
            .map(|(s, e, p, v)| {
                let (b, d) = self.place(s, e);
                (b, d, p, v)
            })
            .collect();
        let first = self.first.unwrap_or(now);
        TakeView {
            track: self.track,
            from: self.song_beat(first),
            around: self.wrap.is_some_and(|len| now - first >= len),
            notes,
        }
    }
}

/// Beats to a millionth (no 3.9999999998 in the project).
fn tidy(b: f64) -> f64 {
    (b * 1e6).round() / 1e6
}

// ---------------------------------------------------------------------------------------------
// devices

/// Keeps the open inputs in line with what is plugged in (and turned on), checking every two seconds.
fn device_thread(weak: Weak<Shared>) {
    loop {
        let Some(sh) = weak.upgrade() else { return };
        scan(&sh, &weak);
        drop(sh);
        std::thread::park_timeout(Duration::from_secs(2));
    }
}

/// The inputs plugged in now, by name ("Keyboard (2)" for a second one with the same name).
fn ports() -> Result<(midir::MidiInput, Vec<(String, midir::MidiInputPort)>), String> {
    let mut input = midir::MidiInput::new("Flux").map_err(|e| e.to_string())?;
    // sysex, clock and active sensing aren't used
    input.ignore(midir::Ignore::All);
    let mut out: Vec<(String, midir::MidiInputPort)> = vec![];
    for p in input.ports() {
        let Ok(base) = input.port_name(&p) else { continue };
        let base = base.trim().to_string();
        let mut name = base.clone();
        let mut k = 2;
        while out.iter().any(|(n, _)| *n == name) {
            name = format!("{base} ({k})");
            k += 1;
        }
        out.push((name, p));
    }
    Ok((input, out))
}

fn connect(weak: Weak<Shared>, id: u32, name: &str) -> Result<midir::MidiInputConnection<()>, String> {
    let (input, ports) = ports()?;
    let port = ports.into_iter().find(|(n, _)| n == name).map(|(_, p)| p).ok_or("it was unplugged")?;
    input
        .connect(
            &port,
            "flux-in",
            move |_, msg, _| {
                if let (Some(sh), Some(ev)) = (weak.upgrade(), parse(msg)) {
                    sh.midi.play(&sh, Input::Device(id), ev);
                }
            },
            (),
        )
        .map_err(|e| e.to_string())
}

fn scan(sh: &Arc<Shared>, weak: &Weak<Shared>) {
    let names: Vec<String> = ports().map(|(_, p)| p.into_iter().map(|(n, _)| n).collect()).unwrap_or_default();
    let m = &sh.midi;
    // connections close outside the lock (closing waits for the input callback, which takes it)
    let mut closing = vec![];
    let mut released = vec![];
    let mut opening = vec![];
    {
        let mut i = m.inner.lock();
        let inner = &mut *i;
        inner.devices.retain_mut(|d| {
            let here = names.contains(&d.name);
            if (!here || !d.on)
                && let Some(c) = d.conn.take()
            {
                closing.push(c);
                released.push(Input::Device(d.id));
            }
            here
        });
        for n in &names {
            if !inner.devices.iter().any(|d| d.name == *n) {
                let id = inner.next_id;
                inner.next_id += 1;
                inner.devices.push(Device { id, name: n.clone(), on: !inner.off.contains(n), conn: None, error: None });
            }
        }
        opening.extend(inner.devices.iter().filter(|d| d.on && d.conn.is_none()).map(|d| (d.id, d.name.clone())));
    }
    drop(closing);
    for from in released {
        m.release(sh, from);
    }
    let mut opened = false;
    for (id, name) in opening {
        let r = connect(weak.clone(), id, &name);
        let mut i = m.inner.lock();
        let Some(d) = i.devices.iter_mut().find(|d| d.id == id) else { continue };
        match r {
            Ok(c) => {
                d.conn = Some(c);
                d.error = None;
                opened = true;
            }
            Err(e) => d.error = Some(e),
        }
    }
    if opened {
        m.ready(sh);
    } else {
        m.update_quick(sh);
    }
}

// ---------------------------------------------------------------------------------------------
// settings

#[derive(Serialize, Deserialize, Default)]
struct Settings {
    /// Inputs turned off.
    #[serde(default)]
    off: Vec<String>,
    #[serde(default)]
    computer_keyboard: bool,
}

impl Settings {
    fn path() -> Option<PathBuf> {
        if cfg!(test) {
            return None;
        }
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Flux").join("midi.json"))
    }

    fn load() -> Settings {
        Settings::path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = Settings::path() else { return };
        let Ok(json) = serde_json::to_vec_pretty(self) else { return };
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let ok = path.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_ok();
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("flux: couldn't save the MIDI settings to {}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::commands;

    fn project(bars: u32, looped: bool) -> Project {
        let mut p = Project::new("midi", 120.0, bars);
        p.looped = looped;
        p
    }

    fn notes_of(part: &Part) -> Vec<(f64, f64, u8, u8)> {
        part.cmds
            .iter()
            .flat_map(|c| match c {
                Command::AddNotes { notes, .. } => notes.iter().map(|n| (n.beat, n.dur, n.pitch.resolve().unwrap(), n.vel)).collect(),
                _ => vec![],
            })
            .collect()
    }

    #[test]
    fn parses_notes_controllers_and_bends_on_any_channel() {
        assert_eq!(parse(&[0x90, 60, 100]), Some(Ev::On(60, 100)));
        assert_eq!(parse(&[0x93, 60, 100]), Some(Ev::On(60, 100)));
        assert_eq!(parse(&[0x90, 60, 0]), Some(Ev::Off(60)), "note-on at velocity 0 is a release");
        assert_eq!(parse(&[0x8F, 61, 40]), Some(Ev::Off(61)));
        assert_eq!(parse(&[0xB0, 64, 127]), Some(Ev::Cc(64, 127)));
        assert_eq!(parse(&[0xE0, 0x00, 0x40]), Some(Ev::Bend(0)));
        assert_eq!(parse(&[0xE5, 0x7F, 0x7F]), Some(Ev::Bend(8191)));
        assert_eq!(parse(&[0xE0, 0x00, 0x00]), Some(Ev::Bend(-8192)));
        assert_eq!(parse(&[0xC0, 5]), None, "program change");
        assert_eq!(parse(&[0xF8]), None, "clock");
        assert_eq!(parse(&[0x90, 60]), None, "cut short");
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn releases_go_where_their_presses_went() {
        let mut r = Router { target: Some(1), ..Default::default() };
        assert_eq!(r.route(Input::Keys, Ev::On(60, 90)), vec![(1, Ev::On(60, 90))]);
        assert_eq!(r.route(Input::Keys, Ev::Cc(64, 127)), vec![(1, Ev::Cc(64, 127))]);
        // another track is selected while the key and the pedal are down
        r.target = Some(2);
        assert_eq!(r.route(Input::Keys, Ev::On(64, 90)), vec![(2, Ev::On(64, 90))]);
        assert_eq!(r.route(Input::Keys, Ev::Off(60)), vec![(1, Ev::Off(60))]);
        assert_eq!(r.route(Input::Keys, Ev::Cc(1, 30)), vec![(2, Ev::Cc(1, 30))]);
        assert_eq!(r.route(Input::Keys, Ev::Cc(64, 0)), vec![(1, Ev::Cc(64, 0)), (2, Ev::Cc(64, 0))]);
        // the same pitch from two inputs: each release finds its own press
        r.route(Input::Device(0), Ev::On(64, 50));
        r.target = Some(3);
        assert_eq!(r.route(Input::Device(0), Ev::Off(64)), vec![(2, Ev::Off(64))]);
        assert_eq!(r.releases(Input::Keys), vec![Ev::Off(64)]);
        // nothing selected: presses go nowhere, a stray release to nothing
        r.target = None;
        assert!(r.route(Input::Mouse, Ev::On(70, 90)).is_empty());
        assert!(r.route(Input::Mouse, Ev::Off(70)).is_empty());
    }

    #[test]
    fn a_take_becomes_notes_controllers_and_bends() {
        // 120 BPM: a beat is half a second
        let mut t = Take::new(7, &project(8, false));
        t.add(Input::Keys, 2.0, Ev::On(60, 96));
        t.add(Input::Keys, 2.1, Ev::Cc(64, 127));
        t.add(Input::Keys, 2.5, Ev::Off(60));
        t.add(Input::Keys, 2.4, Ev::Cc(64, 0));
        t.add(Input::Keys, 3.0, Ev::Bend(4000));
        t.add(Input::Keys, 3.0, Ev::On(67, 80));
        t.add(Input::Keys, 3.2, Ev::On(71, 70));
        t.at(3.75);
        let part = t.drain(true).unwrap();
        assert_eq!(part.track, 7);
        assert_eq!(notes_of(&part), vec![(4.0, 1.0, 60, 96), (6.0, 1.5, 67, 80), (6.4, 1.1, 71, 70)]);
        assert_eq!(part.notes, 3);
        assert_eq!(part.bars, (2, 2));
        let ccs: Vec<_> = part.cmds.iter().filter_map(|c| if let Command::AddCc { cc, points, .. } = c { Some((*cc, points.iter().map(|p| (p.beat, p.value)).collect::<Vec<_>>())) } else { None }).collect();
        assert_eq!(ccs, vec![(64, vec![(4.2, 127), (4.8, 0)])]);
        assert!(part.cmds.iter().any(|c| matches!(c, Command::AddBend { points, .. } if points.len() == 1 && points[0].beat == 6.0 && points[0].value == 4000)));
        assert!(t.drain(true).is_none(), "drained");

        // the commands apply to the project
        let mut p = project(8, false);
        commands::apply(&mut p, &Command::AddTrack { name: "Keys".into(), instrument: flux_core::Instrument::None, color: None }).unwrap();
        let id = p.tracks[0].id;
        let mut t = Take::new(id, &p);
        t.add(Input::Keys, 2.0, Ev::On(60, 96));
        t.add(Input::Keys, 2.5, Ev::Off(60));
        for c in &t.drain(true).unwrap().cmds {
            commands::apply(&mut p, c).unwrap();
        }
        assert_eq!(p.tracks[0].notes.len(), 1);
        assert_eq!((p.tracks[0].notes[0].start, p.tracks[0].notes[0].dur), (4.0, 1.0));
    }

    #[test]
    fn a_looping_take_keeps_notes_across_the_loop_point() {
        // two bars at 120 BPM: the loop is 4 s (8 beats)
        let mut t = Take::new(1, &project(2, true));
        t.at(3.0);
        t.add(Input::Keys, 3.5, Ev::On(60, 100));
        t.add(Input::Keys, 3.9, Ev::On(62, 100));
        // round the loop: the song position starts again from 0
        t.at(0.05);
        assert_eq!(t.pass(), 1);
        // an event from just before the loop point arriving late still lands before it
        t.add(Input::Keys, 3.98, Ev::Off(60));
        t.add(Input::Keys, 0.3, Ev::Off(62));
        t.add(Input::Keys, 1.0, Ev::On(64, 90));
        t.add(Input::Keys, 1.5, Ev::Off(64));
        let part = t.drain(false).unwrap();
        // the note held over the loop point stops at the end of the song
        assert_eq!(notes_of(&part), vec![(7.0, 0.96, 60, 100), (7.8, 0.2, 62, 100), (2.0, 1.0, 64, 90)]);
        assert_eq!(part.bars, (1, 2));
        // the view covers the whole loop once it has gone round
        t.at(3.0);
        t.add(Input::Keys, 3.5, Ev::On(65, 90));
        t.at(1.0);
        let v = t.view();
        assert_eq!(v.from, 6.0);
        assert!(v.around);
        assert_eq!(v.notes, vec![(7.0, 1.0, 65, 90)]);
    }

    #[test]
    fn lists_the_midi_inputs_plugged_in() {
        // read-only: nothing is opened
        let (_, ports) = ports().expect("the system's MIDI service");
        let names: Vec<&String> = ports.iter().map(|(n, _)| n).collect();
        eprintln!("MIDI inputs: {names:?}");
        for (i, n) in names.iter().enumerate() {
            assert!(!n.is_empty());
            assert!(!names[..i].contains(n), "{n} listed twice");
        }
    }

    #[test]
    fn a_mod_wheel_is_thinned() {
        let mut t = Take::new(1, &project(8, false));
        for k in 0..500 {
            t.add(Input::Device(0), 1.0 + k as f64 * 0.001, Ev::Cc(1, (k % 128) as u8));
        }
        t.add(Input::Device(0), 2.0, Ev::Cc(11, 64));
        let n1 = t.ccs.iter().filter(|c| c.1 == 1).count();
        assert!((45..=51).contains(&n1), "{n1} points for half a second");
        // the last value of each stretch is kept
        assert_eq!(t.ccs.iter().rev().find(|c| c.1 == 1).unwrap().2, (499 % 128) as u8);
        assert_eq!(t.ccs.iter().filter(|c| c.1 == 11).count(), 1);
    }
}
