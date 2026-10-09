//! Rendering test notes through sfizz, in parallel, with the results a single sfizz instance would give.
//!
//! A card plays a few hundred single notes. Each note is rendered alone (CCs, optional keyswitch, note-on after a
//! lead-in, note-off, tail) and all voices are cut afterwards. sfizz still carries state from one note to the next:
//! round-robin counters, the active keyswitch, the previous note, CC-triggered regions, and its random generator.
//! The Python reference played everything on two instances in a fixed order (a "scan" instance for the key scan,
//! a "detail" instance for everything else), so a note's sound can depend on every note played before it.
//!
//! [`Rig`] reproduces that while spreading the notes over several instances:
//! - order-independent instruments (no state opcodes) render on any free instance;
//! - stateful instruments split a lane's notes into contiguous chunks; an instance taking a chunk first replays,
//!   without rendering audio, the earlier notes of that lane that change state (keyswitch keys only, or every note
//!   for round robins and the like), so it arrives in exactly the state the single instance would have had;
//! - instruments that draw random numbers run serially on one instance per lane (the random sequence is shared,
//!   so only the reference order reproduces it).
//!
//! Notes longer than the preloaded part of the samples depend on sfizz streaming the rest in time (see
//! [`super::sfizz`]); they render below normal thread priority and pause for [`SETTLE`] after each note event,
//! so sfizz's file dispatcher gets to hand the files to its loaders before the voices need them.

use super::measure::at;
use super::sfizz::{CardSynth as Synth, PRELOAD};
use super::sfztext::Statefulness;
use anyhow::{Context, Result};
use flux_engine::sfizz::Ev;
use rayon::prelude::*;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// Samples between the CCs and the note-on, so CC smoothing has settled.
pub const LEAD: usize = 1024;
/// sfizz block size.
pub const BLOCK: usize = 256;
/// CC values sent before every note (CC1 = 70, CC7 = 100, CC11 = 127).
pub const BASE_CC: [(u8, u8); 3] = [(1, 70), (7, 100), (11, 127)];
/// Notes rendered for longer than this (seconds from the note-on) may outrun the preloaded frames of a sample
/// played at a high rate (a 96 kHz file transposed up), so they wait for sfizz's loader after each event.
pub const LONG_NOTE_S: f64 = PRELOAD as f64 / 96_000.0;
/// Most sample data (bytes) the instances of one card may hold together; big instruments get fewer instances.
pub const MEMORY_BUDGET: u64 = 1 << 30;
/// How long a long note waits after each event.
pub const SETTLE: Duration = Duration::from_millis(30);

/// One test note.
#[derive(Clone, Debug, PartialEq)]
pub struct Play {
    pub key: u8,
    pub vel: u8,
    /// Seconds from note-on to note-off.
    pub hold: f64,
    /// Seconds rendered after the note-off.
    pub tail: f64,
    /// CC values that replace (or add to) [`BASE_CC`].
    pub ccs: Vec<(u8, u8)>,
    /// Stop rendering once no voice is left after the note-off (for release measurements).
    pub stop: bool,
}

impl Play {
    pub fn new(key: u8, vel: u8, hold: f64) -> Play {
        Play { key, vel, hold, tail: 0.05, ccs: vec![], stop: false }
    }

    pub fn with_cc(mut self, cc: u8, value: u8) -> Play {
        self.ccs.push((cc, value));
        self
    }

    fn off(&self) -> usize {
        LEAD + at(self.hold)
    }

    fn total(&self) -> usize {
        self.off() + at(self.tail)
    }

    /// Whether rendering this note may need sample frames beyond the preloaded ones.
    fn is_long(&self) -> bool {
        self.hold + self.tail > LONG_NOTE_S
    }

    /// Rough rendering cost (samples), for load balancing.
    fn cost(&self) -> usize {
        if self.stop { self.off() + at(1.0) } else { self.total() }
    }

    /// The note's events in dispatch order: CCs, keyswitch on/off, note-on, note-off (the reference sorts by time,
    /// then CCs before note-offs before keyswitch note-ons before note-ons).
    fn events(&self, ks: Option<u8>) -> Vec<(usize, Ev)> {
        let mut cc: Vec<(u8, u8)> = BASE_CC.to_vec();
        for &(c, v) in &self.ccs {
            match cc.iter_mut().find(|(k, _)| *k == c) {
                Some(slot) => slot.1 = v,
                None => cc.push((c, v)),
            }
        }
        let mut ev: Vec<(usize, Ev)> = cc.into_iter().map(|(c, v)| (0, Ev::Cc(c, v))).collect();
        if let Some(k) = ks {
            ev.push((LEAD - 400, Ev::On(k, 100)));
            ev.push((LEAD - 200, Ev::Off(k)));
        }
        ev.push((LEAD, Ev::On(self.key, self.vel)));
        ev.push((self.off(), Ev::Off(self.key)));
        let class = |e: &Ev| match e {
            Ev::Cc(..) | Ev::Bend(_) => (0, 1),
            Ev::Off(_) => (1, 1),
            Ev::On(n, _) => (2, if *n >= 120 { 0 } else { 1 }),
        };
        ev.sort_by_key(|(t, e)| (*t, class(e)));
        ev
    }
}

/// Render one note on `synth` and cut all voices. Returns the mono signal from the note-on: `(l + r) / 2` in
/// `f32` (as the reference's numpy mean), then widened.
pub fn render(synth: &mut Synth, play: &Play, ks: Option<u8>) -> Vec<f64> {
    let ev = play.events(ks);
    let total = play.total();
    let stop_after = play.stop.then(|| play.off() + 64);
    let n_blocks = total.div_ceil(BLOCK);
    let mut mono: Vec<f64> = Vec::with_capacity(total.saturating_sub(LEAD));
    let (mut l, mut r) = ([0f32; BLOCK], [0f32; BLOCK]);
    let mut ei = 0;
    let long = play.is_long();
    let _priority = long.then(BelowNormalPriority::enter);
    for bi in 0..n_blocks {
        let start = bi * BLOCK;
        let end = start + BLOCK;
        let mut notes = false;
        while ei < ev.len() && ev[ei].0 < end {
            notes |= matches!(ev[ei].1, Ev::On(..) | Ev::Off(_));
            synth.send(ev[ei].0.saturating_sub(start), ev[ei].1);
            ei += 1;
        }
        synth.render_block(&mut l, &mut r);
        if long && notes {
            // the voices just started have queued their files: let the dispatcher hand them to a loader, so the
            // next block waits for them
            std::thread::sleep(SETTLE);
        }
        let keep = BLOCK.min(total - start);
        for i in 0..keep {
            if start + i >= LEAD {
                mono.push(((l[i] + r[i]) / 2.0) as f64);
            }
        }
        if let Some(s) = stop_after
            && start > s
            && ei >= ev.len()
            && synth.active_voices() == 0
        {
            break;
        }
    }
    synth.all_sound_off();
    mono
}

/// Send a note's events without rendering it, so the instance's state advances as if it had been played.
fn replay(synth: &mut Synth, play: &Play, ks: Option<u8>) {
    for (_, e) in play.events(ks) {
        synth.send(0, e);
    }
    synth.all_sound_off();
    let (mut l, mut r) = ([0f32; BLOCK], [0f32; BLOCK]);
    synth.render_block(&mut l, &mut r);
}

/// The two reference instances: the key scan, and everything after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Scan = 0,
    Detail = 1,
}

struct Slot {
    synth: Synth,
    lane: Option<Lane>,
    /// Notes of `lane` this instance has seen (played or replayed).
    pos: usize,
}

/// Counters for the report.
#[derive(Clone, Copy, Debug, Default)]
pub struct RigStats {
    pub instances: usize,
    pub rendered: usize,
    pub replayed: usize,
}

/// A pool of sfizz instances of one SFZ that renders note lists in reference order semantics (see module docs).
pub struct Rig {
    sfz: PathBuf,
    ks: Option<u8>,
    state: Statefulness,
    workers: AtomicUsize,
    bytes_per_instance: AtomicU64,
    pool: Mutex<Vec<Slot>>,
    logs: Mutex<[Vec<Play>; 2]>,
    regions: AtomicI64,
    instances: AtomicUsize,
    rendered: AtomicUsize,
    replayed: AtomicUsize,
}

impl Rig {
    /// `workers` = most instances used at once (1 = strictly serial, like the reference).
    pub fn new(sfz: &Path, ks: Option<u8>, state: Statefulness, workers: usize) -> Rig {
        Rig {
            sfz: sfz.to_path_buf(),
            ks,
            state,
            workers: AtomicUsize::new(workers.max(1)),
            bytes_per_instance: AtomicU64::new(0),
            pool: Mutex::new(vec![]),
            logs: Mutex::new([vec![], vec![]]),
            regions: AtomicI64::new(-1),
            instances: AtomicUsize::new(0),
            rendered: AtomicUsize::new(0),
            replayed: AtomicUsize::new(0),
        }
    }

    pub fn state(&self) -> &Statefulness {
        &self.state
    }

    /// Regions sfizz loaded (after the first instance is up).
    pub fn regions(&self) -> Option<i64> {
        let n = self.regions.load(Ordering::Relaxed);
        (n >= 0).then_some(n)
    }

    pub fn stats(&self) -> RigStats {
        RigStats {
            instances: self.instances.load(Ordering::Relaxed),
            rendered: self.rendered.load(Ordering::Relaxed),
            replayed: self.replayed.load(Ordering::Relaxed),
        }
    }

    fn load(&self) -> Result<Synth> {
        let s = Synth::new(&self.sfz, super::measure::SR as f32, BLOCK, PRELOAD)?;
        self.regions.store(s.num_regions, Ordering::Relaxed);
        self.instances.fetch_add(1, Ordering::Relaxed);
        Ok(s)
    }

    /// Load the first instance of each lane (one shared instance for order-independent instruments) in parallel,
    /// while `side` runs on another thread. Loading big instruments takes seconds per instance, so this overlaps
    /// it. Fails if any instance fails to load (the reference's load check).
    ///
    /// Also measures what one instance holds in memory and lowers the number of instances so that they stay
    /// within [`MEMORY_BUDGET`].
    pub fn prepare<T: Send>(&self, side: impl FnOnce() -> T + Send) -> Result<T> {
        super::sfizz::pin_loader_pool()?;
        let lanes: Vec<Option<Lane>> = if self.ordered() { vec![Some(Lane::Scan), Some(Lane::Detail)] } else { vec![None] };
        let before = super::sfizz::allocated_bytes();
        let out = std::thread::scope(|s| {
            let side = s.spawn(side);
            let loads: Vec<_> = lanes.iter().map(|&lane| s.spawn(move || self.load().map(|synth| Slot { synth, lane, pos: 0 }))).collect();
            let mut first_err = None;
            for h in loads {
                match h.join() {
                    Ok(Ok(slot)) => self.release(slot),
                    Ok(Err(e)) => first_err = first_err.or(Some(e)),
                    Err(_) => first_err = first_err.or(Some(anyhow::anyhow!("sfizz panicked while loading"))),
                }
            }
            let out = side.join().map_err(|_| anyhow::anyhow!("panicked while loading"))?;
            match first_err {
                Some(e) => Err(e),
                None => Ok(out),
            }
        })?;
        // the counter covers the whole process, so other work only makes this estimate larger (fewer instances)
        let per_instance = match (before, super::sfizz::allocated_bytes()) {
            (Some(b), Some(a)) => a.saturating_sub(b) / lanes.len() as u64,
            _ => u64::MAX,
        };
        self.bytes_per_instance.store(per_instance, Ordering::Relaxed);
        let fit = (MEMORY_BUDGET / per_instance.max(1)).max(1) as usize;
        self.workers.store(self.workers.load(Ordering::Relaxed).min(fit), Ordering::Relaxed);
        Ok(out)
    }

    /// Bytes of sample data one instance holds (known after [`prepare`](Self::prepare)).
    pub fn bytes_per_instance(&self) -> Option<u64> {
        let n = self.bytes_per_instance.load(Ordering::Relaxed);
        (n != 0 && n != u64::MAX).then_some(n)
    }

    fn ordered(&self) -> bool {
        !self.state.is_stateless()
    }

    /// An instance able to play `lane` from note `at` on.
    fn acquire(&self, lane: Lane, at: usize) -> Result<Slot> {
        {
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let pick = if self.ordered() {
                pool.iter()
                    .enumerate()
                    .filter(|(_, s)| (s.lane == Some(lane) || s.lane.is_none()) && s.pos <= at)
                    .max_by_key(|(_, s)| (s.lane.is_some(), s.pos))
                    .map(|(i, _)| i)
            } else {
                pool.iter().position(|s| s.lane == Some(lane)).or((!pool.is_empty()).then_some(0))
            };
            if let Some(i) = pick {
                return Ok(pool.swap_remove(i));
            }
        }
        Ok(Slot { synth: self.load()?, lane: None, pos: 0 })
    }

    fn release(&self, slot: Slot) {
        self.pool.lock().unwrap_or_else(|e| e.into_inner()).push(slot);
    }

    /// Render `plays` (appended to `lane`'s note list) and map each render through `f` on the worker thread.
    /// Results are in input order.
    pub fn run<T, F>(&self, lane: Lane, plays: &[Play], f: F) -> Result<Vec<T>>
    where
        T: Send,
        F: Fn(&Play, Vec<f64>) -> T + Sync,
    {
        if plays.is_empty() {
            return Ok(vec![]);
        }
        let base = {
            let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
            let log = &mut logs[lane as usize];
            let b = log.len();
            log.extend_from_slice(plays);
            b
        };
        let k = if self.state.random { 1 } else { self.workers.load(Ordering::Relaxed).min(plays.len()) };
        let chunks = split(plays, k);
        let run_chunk = |r: &Range<usize>| self.run_chunk(lane, base, plays, r.clone(), &f);
        let parts: Vec<Result<Vec<T>>> =
            if chunks.len() == 1 { vec![run_chunk(&chunks[0])] } else { chunks.par_iter().map(run_chunk).collect() };
        let mut out = Vec::with_capacity(plays.len());
        for p in parts {
            out.extend(p?);
        }
        Ok(out)
    }

    fn run_chunk<T, F>(&self, lane: Lane, base: usize, plays: &[Play], r: Range<usize>, f: &F) -> Result<Vec<T>>
    where
        F: Fn(&Play, Vec<f64>) -> T,
    {
        let start = base + r.start;
        let mut slot = self.acquire(lane, start).with_context(|| format!("loading {}", self.sfz.display()))?;
        if self.ordered() && slot.pos < start {
            let todo: Vec<Play> = {
                let logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
                logs[lane as usize][slot.pos..start].iter().filter(|p| self.state.key_changes_state(p.key as i64)).cloned().collect()
            };
            for p in &todo {
                replay(&mut slot.synth, p, self.ks);
            }
            self.replayed.fetch_add(todo.len(), Ordering::Relaxed);
        }
        let mut out = Vec::with_capacity(r.len());
        for p in &plays[r.clone()] {
            let y = render(&mut slot.synth, p, self.ks);
            out.push(f(p, y));
        }
        self.rendered.fetch_add(r.len(), Ordering::Relaxed);
        slot.lane = Some(lane);
        slot.pos = start + r.len();
        self.release(slot);
        Ok(out)
    }
}

/// Runs the current thread below normal priority until dropped, so sfizz's file dispatcher (normal priority)
/// runs as soon as a note-on wakes it and streamed samples arrive before a voice runs out of preloaded frames
/// (see [`super::sfizz`]). Windows only; elsewhere this does nothing.
struct BelowNormalPriority {
    #[cfg(windows)]
    previous: Option<i32>,
}

#[cfg(windows)]
mod thread_priority {
    use std::ffi::c_void;
    pub const BELOW_NORMAL: i32 = -1;
    pub const ERROR_RETURN: i32 = 0x7fff_ffff;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentThread() -> *mut c_void;
        pub fn GetThreadPriority(thread: *mut c_void) -> i32;
        pub fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32;
    }
}

impl BelowNormalPriority {
    fn enter() -> BelowNormalPriority {
        #[cfg(windows)]
        {
            use thread_priority::*;
            // SAFETY: the pseudo handle of the calling thread is always valid; failures are reported by return value
            unsafe {
                let me = GetCurrentThread();
                let previous = GetThreadPriority(me);
                let lowered = previous != ERROR_RETURN && previous > BELOW_NORMAL && SetThreadPriority(me, BELOW_NORMAL) != 0;
                BelowNormalPriority { previous: lowered.then_some(previous) }
            }
        }
        #[cfg(not(windows))]
        BelowNormalPriority {}
    }
}

impl Drop for BelowNormalPriority {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(p) = self.previous {
            // SAFETY: as in `enter`
            unsafe {
                thread_priority::SetThreadPriority(thread_priority::GetCurrentThread(), p);
            }
        }
    }
}

/// Split `plays` into at most `k` contiguous chunks of similar rendering cost.
fn split(plays: &[Play], k: usize) -> Vec<Range<usize>> {
    let k = k.clamp(1, plays.len().max(1));
    if k == 1 {
        return std::iter::once(0..plays.len()).collect();
    }
    let total: usize = plays.iter().map(Play::cost).sum();
    let target = total.div_ceil(k);
    let mut out = vec![];
    let (mut a, mut acc) = (0, 0);
    for (i, p) in plays.iter().enumerate() {
        acc += p.cost();
        let left = plays.len() - i - 1;
        let chunks_left = k - out.len() - 1;
        if (acc >= target && chunks_left > 0) || left < chunks_left {
            out.push(a..i + 1);
            a = i + 1;
            acc = 0;
        }
    }
    if a < plays.len() {
        out.push(a..plays.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_order_matches_the_reference() {
        let p = Play::new(60, 90, 0.25).with_cc(1, 0);
        let ev = p.events(Some(24));
        let kinds: Vec<String> = ev.iter().map(|(t, e)| format!("{t}:{e:?}")).collect();
        assert_eq!(kinds, vec!["0:Cc(1, 0)", "0:Cc(7, 100)", "0:Cc(11, 127)", "624:On(24, 100)", "824:Off(24)", "1024:On(60, 90)", "12049:Off(60)"]);
        let q = Play::new(60, 90, 0.25).with_cc(64, 127);
        assert!(matches!(q.events(None)[3], (0, Ev::Cc(64, 127))));
        assert_eq!(p.total(), 1024 + 11025 + 2205);
    }

    #[test]
    fn chunks_cover_everything_in_order() {
        for n in [1usize, 2, 5, 17, 128] {
            let plays: Vec<Play> = (0..n).map(|i| Play::new(i as u8, 90, if i % 3 == 0 { 2.0 } else { 0.25 })).collect();
            for k in [1usize, 2, 3, 8, 200] {
                let c = split(&plays, k);
                assert!(c.len() <= k.max(1));
                assert_eq!(c.first().unwrap().start, 0);
                assert_eq!(c.last().unwrap().end, n);
                assert!(c.windows(2).all(|w| w[0].end == w[1].start && !w[0].is_empty()));
            }
        }
    }
}
