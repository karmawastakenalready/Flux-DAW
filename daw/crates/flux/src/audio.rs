//! Playback through the default output device (cpal). Three sources:
//! - Live: the project played in real time by the live engine (edits are heard while playing),
//! - Render: the last offline render (the exact master that analysis and export use),
//! - Clip: an audition clip (A/B comparisons).
//!
//! The stream lives on its own thread (cpal streams are not `Send` on every platform) and is opened
//! lazily on the first `play`, so a headless MCP server never touches the audio device. Live audio is
//! rendered ahead by an engine thread into a short queue; the audio callback only copies from it.
//!
//! Notes played in (a MIDI keyboard: `input`) go to the live engine, which plays them with the song: or, with
//! it stopped (or a render or clip playing), on their own, mixed into whatever plays, until they have rung out.
//! While something can play in (`set_quick`) the queue is kept short so they are heard soon after.

use crate::state::LastRender;
use flux_core::Project;
use flux_engine::live::{LiveEngine, RenderHints, BLOCK};
use flux_engine::sfizz::Ev;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Once, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Live,
    Render,
    Clip,
}

/// A short piece of audio to audition.
pub struct Clip {
    pub sr: u32,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// Identifies the clip (e.g. "audition 3 / B").
    pub key: String,
}

/// Shared between the player, the live engine thread and the audio callback.
struct LiveLink {
    ring: Mutex<VecDeque<(f32, f32, u32)>>,
    pending: Mutex<Option<Arc<Project>>>,
    hints: Mutex<Option<RenderHints>>,
    seek: AtomicU64,
    active: AtomicBool,
    ended: AtomicBool,
    loading: AtomicUsize,
    sr: AtomicU32,
    status: Mutex<Vec<(u64, String)>>,
    levels: Mutex<Vec<(u64, f32)>>,
    /// Bumps on every seek, so the callback drops its interpolation state.
    epoch: AtomicU64,
    /// Played in (track, event), for the engine to hand on.
    input: Mutex<Vec<(u64, Ev)>>,
    /// With the song stopped, the queue holds what is played in (it hasn't rung out yet).
    monitoring: AtomicBool,
    /// Keep the queue short: something can play in.
    quick: AtomicBool,
    /// The callback found the queue empty while sound was going out (the queue grows to keep up).
    underruns: AtomicU64,
    /// The engine thread, woken when something is played in.
    thread: OnceLock<std::thread::Thread>,
}

const NO_SEEK: u64 = u64::MAX;
/// The song position of queued frames played in with the song stopped.
const NO_POS: u32 = u32::MAX;
/// With the song stopped, the engine runs on this long after the last thing played in (tails ring out).
const MONITOR_TAIL: Duration = Duration::from_secs(5);
/// How much live audio is queued: normally, and to start with while something can play in (it grows by
/// `QUICK_STEP` each time the output runs dry, up to `AHEAD`).
const AHEAD: Duration = Duration::from_millis(120);
const QUICK: Duration = Duration::from_millis(25);
const QUICK_STEP: Duration = Duration::from_millis(10);

pub struct Player {
    buf: Mutex<Option<Arc<LastRender>>>,
    /// Playhead in source frames (f64 bits): song frames in Live / Render, clip frames in Clip.
    pos: AtomicU64,
    playing: AtomicBool,
    mode: AtomicU8,
    /// Live or Render: what "play the song" means (Clip mode returns to it).
    song_mode: AtomicU8,
    device_sr: AtomicU32,
    started: Once,
    pub error: Mutex<Option<String>>,
    live: OnceLock<Arc<LiveLink>>,
    /// What the last render measured (project name, hints), handed to the live engine.
    hints: Mutex<Option<(String, RenderHints)>>,
    clip: Mutex<Option<Arc<Clip>>>,
    clip_pos: AtomicU64,
    /// `set_quick`, for an engine started later.
    quick: AtomicBool,
    /// What the speakers get, smoothed: mean square L/R and decaying peak L/R (f32 bits).
    meter: [AtomicU32; 4],
    /// Song second (f64 bits) heard at `heard_at` (ns after `born`), playing live: where a note played in lands.
    heard: AtomicU64,
    heard_at: AtomicU64,
    born: Instant,
}

fn mode_of(v: u8) -> Mode {
    match v {
        1 => Mode::Render,
        2 => Mode::Clip,
        _ => Mode::Live,
    }
}

impl Player {
    pub fn new() -> Arc<Player> {
        Arc::new(Player {
            buf: Mutex::new(None),
            pos: AtomicU64::new(0f64.to_bits()),
            playing: AtomicBool::new(false),
            mode: AtomicU8::new(0),
            song_mode: AtomicU8::new(0),
            device_sr: AtomicU32::new(0),
            started: Once::new(),
            error: Mutex::new(None),
            live: OnceLock::new(),
            hints: Mutex::new(None),
            clip: Mutex::new(None),
            clip_pos: AtomicU64::new(0f64.to_bits()),
            quick: AtomicBool::new(false),
            meter: Default::default(),
            heard: AtomicU64::new(0f64.to_bits()),
            heard_at: AtomicU64::new(0),
            born: Instant::now(),
        })
    }

    /// Play a note or controller on a track's instrument now (a MIDI keyboard's): through the live engine, with
    /// the song or on its own. Starts the engine (with `project()`) and the audio output if they aren't running.
    // (tests play in without an audio device)
    #[cfg_attr(test, allow(dead_code))]
    pub fn input(self: &Arc<Self>, track: u64, ev: Ev, project: impl FnOnce() -> Project) {
        let link = match self.live.get() {
            Some(l) => l.clone(),
            None => self.ensure_live(&project()),
        };
        link.input.lock().push((track, ev));
        if let Some(t) = link.thread.get() {
            t.unpark();
        }
        self.ensure_stream();
    }

    /// Get ready for notes played in: start the live engine (it loads the instruments) and the audio output, so
    /// the first note is heard.
    #[cfg_attr(test, allow(dead_code))]
    pub fn prepare(self: &Arc<Self>, project: impl FnOnce() -> Project) {
        if self.live.get().is_none() {
            self.ensure_live(&project());
        }
        self.ensure_stream();
    }

    /// Something can play in (a MIDI keyboard is connected): keep the live queue short so it is heard soon after.
    /// Otherwise more is queued, which rides out hiccups better.
    pub fn set_quick(&self, on: bool) {
        if let Some(l) = self.live.get() {
            l.quick.store(on, Ordering::Relaxed);
        }
        self.quick.store(on, Ordering::Relaxed);
    }

    /// Notes played in are sounding with the song stopped (or under a render or clip).
    pub fn monitoring(&self) -> bool {
        self.live.get().is_some_and(|l| l.monitoring.load(Ordering::Relaxed))
    }

    /// The song second being heard at `at`, while the song plays live (where a note played then belongs), once the
    /// output has started. It runs on past the end of a loop (the caller wraps it).
    pub fn heard_seconds(&self, at: Instant) -> Option<f64> {
        if !self.is_playing() || self.mode() != Mode::Live {
            return None;
        }
        let ns = self.heard_at.load(Ordering::Relaxed);
        if ns == 0 {
            return None;
        }
        let sec = f64::from_bits(self.heard.load(Ordering::Relaxed));
        let then = self.born + Duration::from_nanos(ns);
        let dt = if at >= then { (at - then).as_secs_f64() } else { -(then - at).as_secs_f64() };
        Some(sec + dt)
    }

    pub fn mode(&self) -> Mode {
        mode_of(self.mode.load(Ordering::Relaxed))
    }

    /// Switch source. Playback stops (except Live <-> Render keeps the playhead).
    pub fn set_mode(&self, m: Mode) {
        if m != Mode::Clip {
            self.song_mode.store(m as u8, Ordering::Relaxed);
        }
        if m == self.mode() {
            return;
        }
        self.stop();
        self.mode.store(m as u8, Ordering::Relaxed);
    }

    /// Live or Render: the mode the song plays in.
    pub fn song_mode(&self) -> Mode {
        mode_of(self.song_mode.load(Ordering::Relaxed))
    }

    /// A render finished: play it in Render mode, and let live playback match its loudness.
    pub fn load(&self, r: Arc<LastRender>) {
        let len = r.out.left.len() as f64;
        let hints = RenderHints::from_render(&r.out);
        *self.hints.lock() = Some((r.project.name.clone(), hints.clone()));
        if let Some(l) = self.live.get() {
            *l.hints.lock() = Some(hints);
        }
        *self.buf.lock() = Some(r);
        if self.mode() == Mode::Render && self.position() >= len {
            self.seek(0.0);
        }
    }

    /// The project changed (an edit): live playback picks it up within a block.
    pub fn project_changed(&self, p: &Project) {
        if let Some(l) = self.live.get() {
            let stale = self.hints.lock().as_ref().is_some_and(|(name, _)| *name != p.name);
            if stale {
                *self.hints.lock() = None;
                *l.hints.lock() = Some(RenderHints::default());
            }
            *l.pending.lock() = Some(Arc::new(p.clone()));
        }
    }

    /// Playhead in source frames.
    pub fn position(&self) -> f64 {
        f64::from_bits(self.pos.load(Ordering::Relaxed))
    }

    fn sr(&self) -> f64 {
        match self.mode() {
            Mode::Live => self.live.get().map(|l| l.sr.load(Ordering::Relaxed)).filter(|&s| s > 0),
            _ => None,
        }
        .or_else(|| self.buf.lock().as_ref().map(|b| b.out.sr))
        .unwrap_or(44100) as f64
    }

    pub fn seconds(&self) -> f64 {
        self.position() / self.sr()
    }

    pub fn seek(&self, frames: f64) {
        let f = frames.max(0.0);
        self.pos.store(f.to_bits(), Ordering::Relaxed);
        if let Some(l) = self.live.get() {
            l.seek.store(f as u64, Ordering::Relaxed);
        }
    }

    pub fn seek_seconds(&self, s: f64) {
        self.seek(s * self.sr());
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn has_audio(&self) -> bool {
        self.buf.lock().is_some()
    }

    /// Instruments the live engine is still loading (playback waits for them).
    pub fn live_loading(&self) -> usize {
        self.live.get().map(|l| l.loading.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// Per-track live state ("live", "frozen", "loading", ...).
    pub fn live_status(&self) -> Vec<(u64, String)> {
        self.live.get().map(|l| l.status.lock().clone()).unwrap_or_default()
    }

    /// Per-track live peak levels (linear).
    pub fn live_levels(&self) -> Vec<(u64, f32)> {
        self.live.get().map(|l| l.levels.lock().clone()).unwrap_or_default()
    }

    /// Start playing the current mode. Live mode needs the project (it starts the engine on first use).
    pub fn play(self: &Arc<Self>, project: Option<&Project>) -> Result<(), String> {
        match self.mode() {
            Mode::Render => {
                if !self.has_audio() {
                    return Err("nothing rendered yet: switch to Live or render first".into());
                }
            }
            Mode::Clip => {
                if self.clip.lock().is_none() {
                    return Err("no clip loaded".into());
                }
            }
            Mode::Live => {
                let p = project.ok_or("live playback needs the project")?;
                let link = self.ensure_live(p);
                *link.pending.lock() = Some(Arc::new(p.clone()));
                link.seek.store(self.position() as u64, Ordering::Relaxed);
                link.ended.store(false, Ordering::Relaxed);
                link.active.store(true, Ordering::Relaxed);
            }
        }
        self.ensure_stream();
        if let Some(e) = self.error.lock().clone() {
            return Err(e);
        }
        self.playing.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
        self.heard_at.store(0, Ordering::Relaxed);
        if let Some(l) = self.live.get() {
            l.active.store(false, Ordering::Relaxed);
        }
    }

    /// Load an audition clip and play it from the start.
    pub fn play_clip(self: &Arc<Self>, clip: Arc<Clip>) -> Result<(), String> {
        self.stop();
        *self.clip.lock() = Some(clip);
        self.clip_pos.store(0f64.to_bits(), Ordering::Relaxed);
        self.mode.store(Mode::Clip as u8, Ordering::Relaxed);
        self.play(None)
    }

    /// The clip playing (or loaded) and how far through it is (0..1).
    pub fn clip_progress(&self) -> Option<(String, f64)> {
        let c = self.clip.lock();
        let c = c.as_ref()?;
        let p = f64::from_bits(self.clip_pos.load(Ordering::Relaxed));
        Some((c.key.clone(), (p / c.left.len().max(1) as f64).clamp(0.0, 1.0)))
    }

    /// Output level per channel as (RMS dBFS over ~300 ms, decaying peak dBFS), in every mode.
    pub fn output_levels(&self) -> [(f64, f64); 2] {
        let g = |i: usize| f32::from_bits(self.meter[i].load(Ordering::Relaxed)) as f64;
        let db = |x: f64| 10.0 * (x + 1e-12).log10();
        [(db(g(0)), db(g(2))), (db(g(1)), db(g(3)))]
    }

    fn update_meter(&self, sr: u32, frames: usize, sq: [f64; 2], pk: [f32; 2]) {
        let dt = frames as f64 / sr.max(1) as f64;
        let a = (-dt / 0.3).exp();
        let fall = 10f64.powf(-2.0 * dt) as f32; // (squared) peaks fall 20 dB per second
        for ch in 0..2 {
            let ms = f32::from_bits(self.meter[ch].load(Ordering::Relaxed)) as f64;
            let ms = ms * a + sq[ch] / frames.max(1) as f64 * (1.0 - a);
            self.meter[ch].store((ms as f32).to_bits(), Ordering::Relaxed);
            let p = f32::from_bits(self.meter[2 + ch].load(Ordering::Relaxed));
            self.meter[2 + ch].store((p * fall).max(pk[ch]).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn device_rate(&self) -> u32 {
        self.device_sr.load(Ordering::Relaxed)
    }

    fn ensure_live(&self, p: &Project) -> Arc<LiveLink> {
        self.live
            .get_or_init(|| {
                let link = Arc::new(LiveLink {
                    ring: Mutex::new(VecDeque::new()),
                    pending: Mutex::new(None),
                    hints: Mutex::new(None),
                    seek: AtomicU64::new(NO_SEEK),
                    active: AtomicBool::new(false),
                    ended: AtomicBool::new(false),
                    loading: AtomicUsize::new(0),
                    sr: AtomicU32::new(p.sample_rate),
                    status: Mutex::new(vec![]),
                    levels: Mutex::new(vec![]),
                    epoch: AtomicU64::new(0),
                    input: Mutex::new(vec![]),
                    monitoring: AtomicBool::new(false),
                    quick: AtomicBool::new(self.quick.load(Ordering::Relaxed)),
                    underruns: AtomicU64::new(0),
                    thread: OnceLock::new(),
                });
                let hints = self.hints.lock().as_ref().filter(|(n, _)| *n == p.name).map(|(_, h)| h.clone()).unwrap_or_default();
                let project = Arc::new(p.clone());
                let l2 = link.clone();
                let _ = std::thread::Builder::new().name("live-engine".into()).spawn(move || engine_thread(l2, project, hints));
                link
            })
            .clone()
    }

    fn ensure_stream(self: &Arc<Self>) {
        self.started.call_once(|| {
            let me = self.clone();
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let spawned = std::thread::Builder::new().name("audio".into()).spawn(move || match open_stream(&me) {
                Ok(stream) => {
                    let _ = tx.send(());
                    let _keep_alive = stream;
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => {
                    *me.error.lock() = Some(format!("audio output unavailable: {e}"));
                    let _ = tx.send(());
                }
            });
            if spawned.is_ok() {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
            }
        });
    }

    /// Produce `frames` stereo frames at `out_sr` (called from the audio callback; never waits long). The first
    /// is heard at `heard`.
    fn fill(&self, cb: &mut CallbackState, out_sr: u32, frames: usize, heard: Instant, mut put: impl FnMut(usize, f32, f32)) {
        let playing = self.playing.load(Ordering::Relaxed);
        if playing && self.mode() == Mode::Live {
            self.fill_live(cb, out_sr, frames, heard, put);
            return;
        }
        // notes played in with the song stopped, under whatever else plays
        let mut mon = std::mem::take(&mut cb.mon);
        mon.clear();
        if let Some(link) = self.live.get().filter(|l| l.monitoring.load(Ordering::Relaxed)) {
            pull(cb, link, out_sr, frames, |_, l, r| mon.push((l, r)));
        }
        let mut put = |i: usize, l: f32, r: f32| {
            let (ml, mr) = mon.get(i).copied().unwrap_or((0.0, 0.0));
            put(i, l + ml, r + mr)
        };
        match self.mode() {
            _ if !playing => (0..frames).for_each(|i| put(i, 0.0, 0.0)),
            Mode::Live => unreachable!(),
            Mode::Render => {
                let guard = self.buf.try_lock();
                match guard.as_ref().and_then(|g| g.as_ref()) {
                    Some(r) => {
                        let o = &r.out;
                        if let Some(p) = play_buffer(&self.pos, &o.left, &o.right, o.sr, o.looped, out_sr, frames, &mut put) {
                            self.playing.store(false, Ordering::Relaxed);
                            self.pos.store(p.to_bits(), Ordering::Relaxed);
                        }
                    }
                    None => (0..frames).for_each(|i| put(i, 0.0, 0.0)),
                }
            }
            Mode::Clip => {
                let guard = self.clip.try_lock();
                match guard.as_ref().and_then(|g| g.as_ref()) {
                    Some(c) => {
                        if play_buffer(&self.clip_pos, &c.left, &c.right, c.sr, false, out_sr, frames, &mut put).is_some() {
                            self.playing.store(false, Ordering::Relaxed);
                        }
                    }
                    None => (0..frames).for_each(|i| put(i, 0.0, 0.0)),
                }
            }
        }
        cb.mon = mon;
    }

    fn fill_live(&self, cb: &mut CallbackState, out_sr: u32, frames: usize, heard: Instant, mut put: impl FnMut(usize, f32, f32)) {
        let Some(link) = self.live.get() else {
            (0..frames).for_each(|i| put(i, 0.0, 0.0));
            return;
        };
        let mut i = pull(cb, link, out_sr, frames, &mut put);
        let last = i.checked_sub(1);
        while i < frames {
            put(i, 0.0, 0.0);
            i += 1;
        }
        if cb.valid && cb.cur.2 != NO_POS && link.seek.load(Ordering::Relaxed) == NO_SEEK {
            self.pos.store((cb.cur.2 as f64).to_bits(), Ordering::Relaxed);
            // the last frame out is heard after those before it
            if let Some(k) = last {
                let at = heard + Duration::from_secs_f64(k as f64 / out_sr.max(1) as f64);
                let sec = cb.cur.2 as f64 / link.sr.load(Ordering::Relaxed).max(1) as f64;
                self.heard.store(sec.to_bits(), Ordering::Relaxed);
                self.heard_at.store((at.saturating_duration_since(self.born).as_nanos() as u64).max(1), Ordering::Relaxed);
            }
        }
        if link.ended.load(Ordering::Relaxed) && link.ring.lock().is_empty() {
            self.playing.store(false, Ordering::Relaxed);
            link.active.store(false, Ordering::Relaxed);
            self.pos.store(0f64.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Up to `frames` frames from the live queue at `out_sr` (interpolated from the engine's rate). Returns how many
/// there were; running dry once sound was flowing counts as an underrun.
fn pull(cb: &mut CallbackState, link: &LiveLink, out_sr: u32, frames: usize, mut put: impl FnMut(usize, f32, f32)) -> usize {
    let epoch = link.epoch.load(Ordering::Relaxed);
    if epoch != cb.epoch {
        cb.epoch = epoch;
        cb.frac = 1.0;
        cb.prev = (0.0, 0.0, 0);
        cb.cur = (0.0, 0.0, 0);
        cb.valid = false;
    }
    let step = link.sr.load(Ordering::Relaxed) as f64 / out_sr.max(1) as f64;
    let mut ring = link.ring.lock();
    let mut i = 0;
    'out: while i < frames {
        while cb.frac >= 1.0 {
            match ring.pop_front() {
                Some(f) => {
                    cb.prev = cb.cur;
                    cb.cur = f;
                    cb.frac -= 1.0;
                    cb.valid = true;
                }
                None => break 'out,
            }
        }
        let f = cb.frac as f32;
        put(i, cb.prev.0 + (cb.cur.0 - cb.prev.0) * f, cb.prev.1 + (cb.cur.1 - cb.prev.1) * f);
        cb.frac += step;
        i += 1;
    }
    if i < frames && cb.valid {
        link.underruns.fetch_add(1, Ordering::Relaxed);
    }
    i
}

/// Interpolation state owned by the audio callback.
struct CallbackState {
    frac: f64,
    prev: (f32, f32, u32),
    cur: (f32, f32, u32),
    epoch: u64,
    /// `cur` holds a real frame (not the reset state after a seek).
    valid: bool,
    /// Notes played in with the song stopped, for this callback (reused).
    mon: Vec<(f32, f32)>,
}

impl CallbackState {
    fn new() -> CallbackState {
        CallbackState { frac: 1.0, prev: (0.0, 0.0, 0), cur: (0.0, 0.0, 0), epoch: 0, valid: false, mon: Vec::with_capacity(4096) }
    }
}

/// Play a buffer from `pos` (f64 frames); returns the final position when a non-looping buffer ran out.
fn play_buffer(
    pos_cell: &AtomicU64,
    left: &[f32],
    right: &[f32],
    sr: u32,
    looped: bool,
    out_sr: u32,
    frames: usize,
    put: &mut impl FnMut(usize, f32, f32),
) -> Option<f64> {
    let n = left.len();
    let p0 = pos_cell.load(Ordering::Relaxed);
    let mut pos = f64::from_bits(p0);
    let step = sr as f64 / out_sr.max(1) as f64;
    let mut ended = None;
    let mut i = 0;
    while i < frames {
        if pos >= n as f64 {
            if looped && n > 0 {
                pos %= n as f64;
            } else {
                pos = 0.0;
                ended = Some(0.0);
                break;
            }
        }
        let k = pos as usize;
        let k1 = if k + 1 < n { k + 1 } else if looped { 0 } else { k };
        let f = (pos - k as f64) as f32;
        put(i, left[k] + (left[k1] - left[k]) * f, right[k] + (right[k1] - right[k]) * f);
        pos += step;
        i += 1;
    }
    while i < frames {
        put(i, 0.0, 0.0);
        i += 1;
    }
    // a seek from another thread during this callback wins
    let _ = pos_cell.compare_exchange(p0, pos.to_bits(), Ordering::Relaxed, Ordering::Relaxed);
    ended
}

/// Live plugins are unloaded once playback has been idle this long (each holds memory and a process of
/// its own); they load again when it resumes.
const RELEASE_AFTER: Duration = Duration::from_secs(600);

/// Runs the live engine: applies edits, seeks and render hints, hands on what is played in, and keeps audio
/// queued (~120 ms, or less while something can play in) while playing, or while notes played in with the song
/// stopped ring. Playback holds until the instruments have loaded (up to 30 s) so everything starts together.
fn engine_thread(link: Arc<LiveLink>, project: Arc<Project>, hints: RenderHints) {
    // decaying filters, reverbs and amp models go subnormal in silence: slow on x86
    let _ftz = flux_engine::denormals::FlushDenormals::new();
    let _ = link.thread.set(std::thread::current());
    let mut eng = LiveEngine::new(project, hints);
    let (mut l, mut r, mut pos) = (vec![0f32; BLOCK], vec![0f32; BLOCK], vec![0u32; BLOCK]);
    let mut activated: Option<Instant> = None;
    let mut last_status = Instant::now() - Duration::from_secs(1);
    let mut idle_since = Instant::now();
    let mut last_release = Instant::now();
    // keys down (track, pitch) and when something was last played in
    let mut held: Vec<(u64, u8)> = vec![];
    let mut last_input: Option<Instant> = None;
    let (mut rolled, mut monitored) = (false, false);
    let (mut quick, mut underruns) = (QUICK, 0);
    loop {
        if let Some(p) = link.pending.lock().take() {
            eng.update(p);
            link.sr.store(eng.sample_rate(), Ordering::Relaxed);
        }
        if let Some(h) = link.hints.lock().take() {
            eng.set_hints(h);
        }
        let s = link.seek.swap(NO_SEEK, Ordering::Relaxed);
        if s != NO_SEEK {
            eng.seek(s as usize);
            link.ring.lock().clear();
            link.epoch.fetch_add(1, Ordering::Relaxed);
        }
        let input = std::mem::take(&mut *link.input.lock());
        if !input.is_empty() {
            last_input = Some(Instant::now());
        }
        for (track, ev) in input {
            match ev {
                Ev::On(p, _) => held.push((track, p)),
                Ev::Off(p) => {
                    if let Some(k) = held.iter().position(|&h| h == (track, p)) {
                        held.swap_remove(k);
                    }
                }
                // all sound / all notes off
                Ev::Cc(120 | 123, _) => held.retain(|h| h.0 != track),
                _ => {}
            }
            eng.input(track, ev);
        }
        eng.poll();
        let loading = eng.loading();
        link.loading.store(loading, Ordering::Relaxed);
        if last_status.elapsed() > Duration::from_millis(40) {
            *link.status.lock() = eng.status();
            *link.levels.lock() = eng.levels();
            last_status = Instant::now();
        }
        // the queue: short while something can play in (longer each time the output ran dry)
        let u = link.underruns.load(Ordering::Relaxed);
        if u != underruns {
            underruns = u;
            quick = (quick + QUICK_STEP).min(AHEAD);
        }
        let ahead = (eng.sample_rate() as f64 * if link.quick.load(Ordering::Relaxed) { quick } else { AHEAD }.as_secs_f64()) as usize;
        let rolling = link.active.load(Ordering::Relaxed);
        if rolled && !rolling {
            // the song stopped: what it queued goes (notes played in ring on from here)
            link.ring.lock().clear();
            link.epoch.fetch_add(1, Ordering::Relaxed);
        }
        rolled = rolling;
        if !rolling {
            activated = None;
            let monitoring = !held.is_empty() || last_input.is_some_and(|t| t.elapsed() < MONITOR_TAIL);
            link.monitoring.store(monitoring, Ordering::Relaxed);
            if monitored && !monitoring {
                // rung out: the next note starts on a fresh queue
                link.ring.lock().clear();
                link.epoch.fetch_add(1, Ordering::Relaxed);
            }
            monitored = monitoring;
            if monitoring {
                idle_since = Instant::now();
                if link.ring.lock().len() >= ahead {
                    std::thread::park_timeout(Duration::from_millis(1));
                    continue;
                }
                let n = eng.render_input(&mut l, &mut r);
                let mut ring = link.ring.lock();
                for i in 0..n {
                    ring.push_back((l[i], r[i], NO_POS));
                }
                continue;
            }
            // checked now and then: a plugin whose window is open stays until the window closes
            if idle_since.elapsed() >= RELEASE_AFTER && last_release.elapsed() >= Duration::from_secs(5) {
                eng.release_plugins();
                last_release = Instant::now();
            }
            std::thread::park_timeout(Duration::from_millis(8));
            continue;
        }
        link.monitoring.store(false, Ordering::Relaxed);
        monitored = false;
        if activated.is_none() {
            eng.wake();
        }
        idle_since = Instant::now();
        let since = *activated.get_or_insert_with(Instant::now);
        // (again: waking may have started loads)
        if eng.loading() > 0 && since.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        if link.ring.lock().len() >= ahead || eng.ended {
            if eng.ended {
                link.ended.store(true, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        let n = eng.render(&mut l, &mut r, &mut pos);
        let mut ring = link.ring.lock();
        for i in 0..n {
            ring.push_back((l[i], r[i], pos[i]));
        }
    }
}

fn open_stream(p: &Arc<Player>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or("no default output device")?;
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let sr = cfg.sample_rate();
    let ch = cfg.channels() as usize;
    p.device_sr.store(sr, Ordering::Relaxed);
    let config = cfg.config();
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&dev, config, p.clone(), sr, ch),
        cpal::SampleFormat::I16 => build::<i16>(&dev, config, p.clone(), sr, ch),
        cpal::SampleFormat::I32 => build::<i32>(&dev, config, p.clone(), sr, ch),
        cpal::SampleFormat::U16 => build::<u16>(&dev, config, p.clone(), sr, ch),
        other => return Err(format!("unsupported output sample format {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn build<T>(dev: &cpal::Device, config: cpal::StreamConfig, p: Arc<Player>, sr: u32, ch: usize) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let ch = ch.max(1);
    let mut cb = CallbackState::new();
    dev.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / ch;
            let (mut sq, mut pk) = ([0f64; 2], [0f32; 2]);
            let ts = info.timestamp();
            let heard = Instant::now() + ts.playback.duration_since(ts.callback);
            p.fill(&mut cb, sr, frames, heard, |i, l, r| {
                sq[0] += (l * l) as f64;
                sq[1] += (r * r) as f64;
                pk[0] = pk[0].max(l.abs());
                pk[1] = pk[1].max(r.abs());
                let f = &mut data[i * ch..(i + 1) * ch];
                f[0] = T::from_sample(l.clamp(-1.0, 1.0));
                if ch > 1 {
                    f[1] = T::from_sample(r.clamp(-1.0, 1.0));
                }
                for x in f.iter_mut().skip(2) {
                    *x = T::EQUILIBRIUM;
                }
            });
            // peaks are stored squared so both meters share the 10*log10 path
            p.update_meter(sr, frames, sq, [pk[0] * pk[0], pk[1] * pk[1]]);
        },
        |e| eprintln!("flux: audio stream error: {e}"),
        None,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    //! The live engine playing notes in, heard through the output callback's code (no audio device: the test pulls
    //! the output at the pace a device would). Needs sfizz and the VSCO 2 cello ensemble; skipped otherwise.

    use super::*;
    use flux_core::{Command, commands};

    const CELLO: &str = "instruments/VSCO-2-CE/CelloEnsSusVib.sfz";

    /// `secs` of output from callbacks of 480 frames at 48 kHz, paced like a device; the left channel.
    fn pull_out(p: &Player, cb: &mut CallbackState, secs: f64) -> Vec<f32> {
        let mut out = vec![];
        for _ in 0..(secs * 100.0).round() as usize {
            std::thread::sleep(Duration::from_millis(10));
            p.fill(cb, 48_000, 480, Instant::now(), |_, l, _| out.push(l));
        }
        out
    }

    fn rms_db(x: &[f32]) -> f64 {
        10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).max(1e-20).log10()
    }

    #[test]
    fn notes_played_in_are_heard_stopped_and_playing() {
        if !flux_engine::paths::sfizz_dll().is_file() || !flux_engine::paths::resolve(CELLO).is_file() {
            eprintln!("skipped: needs sfizz and {CELLO}");
            return;
        }
        // two bars at 120 BPM, dry, nothing in the first
        let mut proj = Project::new("play-in", 120.0, 2);
        proj.sample_rate = 48_000;
        let cmds = serde_json::json!([
            {"op": "add_track", "name": "Cello", "instrument": {"kind": "sfz", "path": CELLO}},
            {"op": "set_mixer", "track": "Cello", "mixer": {"reverb_send": 0}},
        ]);
        for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
            commands::apply(&mut proj, &c).unwrap();
        }
        let id = proj.tracks[0].id;
        let p = Player::new();
        let link = p.ensure_live(&proj);
        let t0 = Instant::now();
        while p.live_status().first().is_none_or(|s| s.1 != "live") && t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(p.live_status()[0].1, "live");
        let play_in = |ev: Ev| {
            link.input.lock().push((id, ev));
            link.thread.get().unwrap().unpark();
        };
        let mut cb = CallbackState::new();

        // stopped: silence, then the key while it is held, then it dies away and the engine stops rendering
        assert!(rms_db(&pull_out(&p, &mut cb, 0.2)) < -120.0);
        play_in(Ev::On(48, 100));
        let held = pull_out(&p, &mut cb, 0.8);
        assert!(p.monitoring());
        let level = rms_db(&held[24_000..]);
        assert!(level > -45.0, "{level:.1} dB while the key is held");
        play_in(Ev::Off(48));
        let after = pull_out(&p, &mut cb, 5.4);
        let tail = rms_db(&after[(2.0 * 48_000.0) as usize..(2.5 * 48_000.0) as usize]);
        assert!(tail < level - 40.0, "{tail:.1} dB after letting go");
        assert!(!p.monitoring(), "rung out");
        assert!(after[after.len() - 4800..].iter().all(|&x| x == 0.0));
        assert_eq!(p.position(), 0.0, "the song stood still");
        assert_eq!(p.heard_seconds(Instant::now()), None);

        // playing live (as `play` starts it): the key sounds over the empty bar, and the song position heard now
        // follows the playhead
        p.playing.store(true, Ordering::Relaxed);
        link.seek.store(0, Ordering::Relaxed);
        link.active.store(true, Ordering::Relaxed);
        pull_out(&p, &mut cb, 0.4);
        play_in(Ev::On(55, 100));
        let level = rms_db(&pull_out(&p, &mut cb, 0.6)[14_400..]);
        assert!(level > -45.0, "{level:.1} dB while the key is held");
        let heard = p.heard_seconds(Instant::now()).unwrap();
        assert!((heard - p.seconds()).abs() < 0.05, "heard {heard:.3} s, playhead {:.3} s", p.seconds());
        assert!((0.9..1.3).contains(&heard), "{heard:.3} s in");

        // stopped with the key still down: the song's queue goes, the key rings on, the playhead stays put
        p.stop();
        let at = p.position();
        let on = rms_db(&pull_out(&p, &mut cb, 0.4)[9_600..]);
        assert!(p.monitoring());
        assert!(on > -45.0, "{on:.1} dB from the key held after stopping");
        assert_eq!(p.position(), at);
        play_in(Ev::Off(55));
    }
}
