//! The sfizz instances cards render with.
//!
//! [`flux_engine::sfizz::Synth`] always loads with sfizz's default preload size and does not expose the MIDNAM
//! export, so this module binds the functions cards need from the same library (loading it again only adds a
//! reference to the module already in the process; sfizz's global state, including its random generator, is
//! shared).
//!
//! # Why cards set a large preload size
//!
//! sfizz keeps the first `preload` frames of every sample in memory and streams the rest when a voice starts: the
//! note-on queues the file, a dispatcher thread hands it to a loader thread, and in freewheeling mode every
//! `render_block` waits for the loads already handed over. If the dispatcher has not run by the time a voice has
//! played its preloaded frames, the voice ends right there. With the default 8192 frames (0.19 s) a busy CPU
//! truncates notes at random: the Python reference shows it as notes that change from run to run, and a sustained
//! note can come out 0.2 s long. Cards preload [`PRELOAD`] frames, enough for every test note except the long
//! sustain and release notes, which [`super::play`] renders with a pause after each event so the loader runs.
//!
//! # Creating instances on several threads
//!
//! sfizz 1.2 shares one loader thread pool between instances through a global `weak_ptr` it reads without a
//! lock and writes only when no instance holds the pool, so two instances created at once while none exists can
//! crash the process. [`pin_loader_pool`] keeps one idle instance alive for the life of the process; after that,
//! creating instances only reads the pointer.

use anyhow::{Context, Result, anyhow, bail};
use flux_engine::sfizz::Ev;
use libloading::Library;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::Path;
use std::sync::OnceLock;

/// Frames of every sample kept in memory (2^17: 2.97 s at 44.1 kHz, 1.37 s at 96 kHz).
pub const PRELOAD: u32 = 1 << 17;

type FCreate = unsafe extern "C" fn() -> *mut c_void;
type FPtr = unsafe extern "C" fn(*mut c_void);
type FLoad = unsafe extern "C" fn(*mut c_void, *const c_char) -> bool;
type FSetF = unsafe extern "C" fn(*mut c_void, f32);
type FSetI = unsafe extern "C" fn(*mut c_void, c_int);
type FSetU = unsafe extern "C" fn(*mut c_void, c_uint);
type FGetI = unsafe extern "C" fn(*mut c_void) -> c_int;
type FMsg3 = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int);
type FMsg2 = unsafe extern "C" fn(*mut c_void, c_int, c_int);
type FRender = unsafe extern "C" fn(*mut c_void, *mut *mut f32, c_int, c_int);
type FMidnam = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;

struct Api {
    _lib: Library,
    create: FCreate,
    free: FPtr,
    load: FLoad,
    set_sr: FSetF,
    set_block: FSetI,
    set_voices: FSetI,
    set_preload: FSetU,
    freewheel: FPtr,
    num_regions: FGetI,
    active_voices: FGetI,
    num_bytes: FGetI,
    note_on: FMsg3,
    note_off: FMsg3,
    cc: FMsg3,
    pitch_wheel: FMsg2,
    render: FRender,
    all_sound_off: FPtr,
    midnam: FMidnam,
    free_memory: FPtr,
}

static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api> {
    API.get_or_init(|| {
        let path = flux_engine::paths::sfizz_dll();
        // SAFETY: loading sfizz runs no initialisation beyond the C runtime's; the symbols are the C API's, with
        // the signatures of sfizz.h
        unsafe {
            let lib = Library::new(&path).map_err(|e| format!("loading {}: {e}", path.display()))?;
            macro_rules! sym {
                ($t:ty, $n:literal) => {
                    *lib.get::<$t>($n).map_err(|e| format!("sfizz symbol {}: {e}", String::from_utf8_lossy($n)))?
                };
            }
            Ok(Api {
                create: sym!(FCreate, b"sfizz_create_synth"),
                free: sym!(FPtr, b"sfizz_free"),
                load: sym!(FLoad, b"sfizz_load_file"),
                set_sr: sym!(FSetF, b"sfizz_set_sample_rate"),
                set_block: sym!(FSetI, b"sfizz_set_samples_per_block"),
                set_voices: sym!(FSetI, b"sfizz_set_num_voices"),
                set_preload: sym!(FSetU, b"sfizz_set_preload_size"),
                freewheel: sym!(FPtr, b"sfizz_enable_freewheeling"),
                num_regions: sym!(FGetI, b"sfizz_get_num_regions"),
                active_voices: sym!(FGetI, b"sfizz_get_num_active_voices"),
                num_bytes: sym!(FGetI, b"sfizz_get_num_bytes"),
                note_on: sym!(FMsg3, b"sfizz_send_note_on"),
                note_off: sym!(FMsg3, b"sfizz_send_note_off"),
                cc: sym!(FMsg3, b"sfizz_send_cc"),
                pitch_wheel: sym!(FMsg2, b"sfizz_send_pitch_wheel"),
                render: sym!(FRender, b"sfizz_render_block"),
                all_sound_off: sym!(FPtr, b"sfizz_all_sound_off"),
                midnam: sym!(FMidnam, b"sfizz_export_midnam"),
                free_memory: sym!(FPtr, b"sfizz_free_memory"),
                _lib: lib,
            })
        }
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

/// Keep sfizz's shared loader thread pool alive for the rest of the process (see the module docs). Call it before
/// creating instances on several threads; later calls do nothing.
pub fn pin_loader_pool() -> Result<()> {
    pinned().map(|_| ())
}

/// The idle instance [`pin_loader_pool`] keeps (as an address, so it can live in a static).
fn pinned() -> Result<usize> {
    static PIN: OnceLock<std::result::Result<usize, String>> = OnceLock::new();
    PIN.get_or_init(|| {
        let api = api().map_err(|e| format!("{e:#}"))?;
        // SAFETY: plain constructor; the instance is deliberately never freed
        let h = unsafe { (api.create)() };
        if h.is_null() { Err("sfizz could not create an instance".into()) } else { Ok(h as usize) }
    })
    .as_ref()
    .copied()
    .map_err(|e| anyhow!("{e}"))
}

/// Bytes sfizz has allocated for buffers in this process, all instances together (`sfizz_get_num_bytes`), or
/// `None` if unknown (the counter is a C `int` and reads negative past 2 GiB).
pub fn allocated_bytes() -> Option<u64> {
    let api = api().ok()?;
    let h = pinned().ok()?;
    // SAFETY: a read-only query; the counter is global, the pinned instance only serves as a valid handle
    let n = unsafe { (api.num_bytes)(h as *mut c_void) };
    u64::try_from(n).ok()
}

/// One sfizz instance set up for measuring: 44.1 kHz style offline rendering (freewheeling), 256 voices and the
/// given preload size.
pub struct CardSynth {
    api: &'static Api,
    h: *mut c_void,
    block: usize,
    /// Regions sfizz loaded.
    pub num_regions: i64,
}

// SAFETY: an sfizz instance may be used from any thread as long as only one thread uses it at a time, which
// `&mut self` on every call guarantees.
unsafe impl Send for CardSynth {}

impl CardSynth {
    /// Create an instance and load `sfz` into it.
    pub fn new(sfz: &Path, sr: f32, block: usize, preload: u32) -> Result<CardSynth> {
        let api = api()?;
        let path = CString::new(sfz.to_string_lossy().as_bytes()).context("SFZ path contains a NUL byte")?;
        // SAFETY: C API calls on a fresh, non-null handle, freed on every error path (or by Drop)
        unsafe {
            let h = (api.create)();
            if h.is_null() {
                bail!("sfizz could not create an instance");
            }
            (api.set_sr)(h, sr);
            (api.set_block)(h, block as c_int);
            (api.freewheel)(h);
            (api.set_voices)(h, 256);
            (api.set_preload)(h, preload);
            if !(api.load)(h, path.as_ptr()) {
                (api.free)(h);
                bail!("sfizz could not load {}", sfz.display());
            }
            let num_regions = i64::from((api.num_regions)(h));
            Ok(CardSynth { api, h, block, num_regions })
        }
    }

    /// Queue one event `delay` frames into the next [`render_block`](Self::render_block).
    pub fn send(&mut self, delay: usize, ev: Ev) {
        let d = delay as c_int;
        // SAFETY: valid handle, exclusive access
        unsafe {
            match ev {
                Ev::On(n, v) => (self.api.note_on)(self.h, d, c_int::from(n), c_int::from(v)),
                Ev::Off(n) => (self.api.note_off)(self.h, d, c_int::from(n), 0),
                Ev::Cc(c, v) => (self.api.cc)(self.h, d, c_int::from(c), c_int::from(v)),
                Ev::Bend(b) => (self.api.pitch_wheel)(self.h, d, c_int::from(b)),
            }
        }
    }

    /// Render one block into `l` and `r` (overwritten; at most the block size).
    pub fn render_block(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len()).min(self.block);
        l[..n].fill(0.0);
        r[..n].fill(0.0);
        let mut ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        // SAFETY: two channel pointers of at least `n` frames each
        unsafe { (self.api.render)(self.h, ptrs.as_mut_ptr(), 2, n as c_int) };
    }

    /// Cut every voice at once.
    pub fn all_sound_off(&mut self) {
        // SAFETY: valid handle, exclusive access
        unsafe { (self.api.all_sound_off)(self.h) }
    }

    pub fn active_voices(&self) -> i32 {
        // SAFETY: read-only query on a valid handle
        unsafe { (self.api.active_voices)(self.h) }
    }
}

impl Drop for CardSynth {
    fn drop(&mut self) {
        // SAFETY: the handle came from `sfizz_create_synth` and is freed once
        unsafe { (self.api.free)(self.h) }
    }
}

/// The SFZ's MIDNAM XML as sfizz exports it (loaded into a separate instance with a tiny preload size).
pub fn export_midnam(sfz: &Path) -> Result<String> {
    let api = api()?;
    let path = CString::new(sfz.to_string_lossy().as_bytes()).context("SFZ path contains a NUL byte")?;
    let model = CString::new("card")?;
    // SAFETY: C API calls on a fresh handle, freed before returning; the exported string is released with
    // sfizz's own allocator
    unsafe {
        let h = (api.create)();
        if h.is_null() {
            bail!("sfizz could not create an instance");
        }
        (api.set_sr)(h, 44100.0);
        (api.set_block)(h, 256);
        (api.set_preload)(h, 1024);
        if !(api.load)(h, path.as_ptr()) {
            (api.free)(h);
            bail!("sfizz could not load {}", sfz.display());
        }
        let p = (api.midnam)(h, model.as_ptr());
        let xml = if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() };
        if !p.is_null() {
            (api.free_memory)(p.cast());
        }
        (api.free)(h);
        Ok(xml)
    }
}
