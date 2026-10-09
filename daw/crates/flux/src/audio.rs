//! Playback through the default output device (cpal). Three sources:
//! - Live: the project played in real time by the live engine (edits are heard while playing),
//! - Render: the last offline render (the exact master that analysis and export use),
//! - Clip: an audition clip (A/B comparisons).
//!
//! The stream lives on its own thread (cpal streams are not `Send` on every platform) and is opened
//! lazily on the first `play`, so a headless MCP server never touches the audio device. Live audio is
//! rendered ahead by an engine thread into a short queue; the audio callback only copies from it.

use crate::state::LastRender;
use flux_core::Project;
use flux_engine::live::{LiveEngine, RenderHints, BLOCK};
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
}

const NO_SEEK: u64 = u64::MAX;

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
    /// What the speakers get, smoothed: mean square L/R and decaying peak L/R (f32 bits).
    meter: [AtomicU32; 4],
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
            meter: Default::default(),
        })
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

    /// Produce `frames` stereo frames at `out_sr` (called from the audio callback; never waits long).
    fn fill(&self, cb: &mut CallbackState, out_sr: u32, frames: usize, mut put: impl FnMut(usize, f32, f32)) {
        if !self.playing.load(Ordering::Relaxed) {
            (0..frames).for_each(|i| put(i, 0.0, 0.0));
            return;
        }
        match self.mode() {
            Mode::Live => self.fill_live(cb, out_sr, frames, put),
            Mode::Render => {
                let guard = self.buf.try_lock();
                let Some(r) = guard.as_ref().and_then(|g| g.as_ref()) else {
                    (0..frames).for_each(|i| put(i, 0.0, 0.0));
                    return;
                };
                let o = &r.out;
                if let Some(p) = play_buffer(&self.pos, &o.left, &o.right, o.sr, o.looped, out_sr, frames, &mut put) {
                    self.playing.store(false, Ordering::Relaxed);
                    self.pos.store(p.to_bits(), Ordering::Relaxed);
                }
            }
            Mode::Clip => {
                let guard = self.clip.try_lock();
                let Some(c) = guard.as_ref().and_then(|g| g.as_ref()) else {
                    (0..frames).for_each(|i| put(i, 0.0, 0.0));
                    return;
                };
                if play_buffer(&self.clip_pos, &c.left, &c.right, c.sr, false, out_sr, frames, &mut put).is_some() {
                    self.playing.store(false, Ordering::Relaxed);
                }
            }
        }
    }

    fn fill_live(&self, cb: &mut CallbackState, out_sr: u32, frames: usize, mut put: impl FnMut(usize, f32, f32)) {
        let Some(link) = self.live.get() else {
            (0..frames).for_each(|i| put(i, 0.0, 0.0));
            return;
        };
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
        drop(ring);
        while i < frames {
            put(i, 0.0, 0.0);
            i += 1;
        }
        if cb.valid && link.seek.load(Ordering::Relaxed) == NO_SEEK {
            self.pos.store((cb.cur.2 as f64).to_bits(), Ordering::Relaxed);
        }
        if link.ended.load(Ordering::Relaxed) && link.ring.lock().is_empty() {
            self.playing.store(false, Ordering::Relaxed);
            link.active.store(false, Ordering::Relaxed);
            self.pos.store(0f64.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Interpolation state owned by the audio callback.
struct CallbackState {
    frac: f64,
    prev: (f32, f32, u32),
    cur: (f32, f32, u32),
    epoch: u64,
    /// `cur` holds a real frame (not the reset state after a seek).
    valid: bool,
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

/// Runs the live engine: applies edits, seeks and render hints, and keeps ~120 ms of audio queued while
/// playing. Playback holds until the instruments have loaded (up to 30 s) so everything starts together.
fn engine_thread(link: Arc<LiveLink>, project: Arc<Project>, hints: RenderHints) {
    // decaying filters, reverbs and amp models go subnormal in silence: slow on x86
    let _ftz = flux_engine::denormals::FlushDenormals::new();
    let mut eng = LiveEngine::new(project, hints);
    let (mut l, mut r, mut pos) = (vec![0f32; BLOCK], vec![0f32; BLOCK], vec![0u32; BLOCK]);
    let mut activated: Option<Instant> = None;
    let mut last_status = Instant::now() - Duration::from_secs(1);
    let mut idle_since = Instant::now();
    let mut last_release = Instant::now();
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
        eng.poll();
        let loading = eng.loading();
        link.loading.store(loading, Ordering::Relaxed);
        if last_status.elapsed() > Duration::from_millis(40) {
            *link.status.lock() = eng.status();
            *link.levels.lock() = eng.levels();
            last_status = Instant::now();
        }
        if !link.active.load(Ordering::Relaxed) {
            activated = None;
            // checked now and then: a plugin whose window is open stays until the window closes
            if idle_since.elapsed() >= RELEASE_AFTER && last_release.elapsed() >= Duration::from_secs(5) {
                eng.release_plugins();
                last_release = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }
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
        let ahead = (eng.sample_rate() as f64 * 0.12) as usize;
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
    let mut cb = CallbackState { frac: 1.0, prev: (0.0, 0.0, 0), cur: (0.0, 0.0, 0), epoch: 0, valid: false };
    dev.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / ch;
            let (mut sq, mut pk) = ([0f64; 2], [0f32; 2]);
            p.fill(&mut cb, sr, frames, |i, l, r| {
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
