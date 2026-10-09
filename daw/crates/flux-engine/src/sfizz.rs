//! Minimal binding to the sfizz C API (loaded at runtime from sfizz.dll / libsfizz).

use anyhow::{anyhow, Result};
use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::OnceLock;

type FCreate = unsafe extern "C" fn() -> *mut c_void;
type FPtr = unsafe extern "C" fn(*mut c_void);
type FLoad = unsafe extern "C" fn(*mut c_void, *const c_char) -> bool;
type FLoadString = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> bool;
type FSetF = unsafe extern "C" fn(*mut c_void, f32);
type FSetI = unsafe extern "C" fn(*mut c_void, c_int);
type FGetI = unsafe extern "C" fn(*mut c_void) -> c_int;
type FMsg3 = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int);
type FMsg2 = unsafe extern "C" fn(*mut c_void, c_int, c_int);
type FRender = unsafe extern "C" fn(*mut c_void, *mut *mut f32, c_int, c_int);

pub struct Api {
    _lib: Library,
    create: FCreate,
    free: FPtr,
    load: FLoad,
    load_string: Option<FLoadString>,
    set_sr: FSetF,
    set_block: FSetI,
    set_voices: FSetI,
    freewheel: FPtr,
    num_regions: FGetI,
    active_voices: FGetI,
    note_on: FMsg3,
    note_off: FMsg3,
    cc: FMsg3,
    pitch_wheel: FMsg2,
    render: FRender,
    all_sound_off: Option<FPtr>,
    set_tuning_frequency: Option<FSetF>,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();

pub fn api() -> Result<&'static Api> {
    API.get_or_init(|| unsafe {
        let path = crate::paths::sfizz_dll();
        let lib = Library::new(&path).map_err(|e| format!("loading {}: {e}", path.display()))?;
        macro_rules! sym {
            ($t:ty, $n:literal) => {
                *lib.get::<$t>($n).map_err(|e| format!("sfizz symbol {}: {e}", String::from_utf8_lossy($n)))?
            };
        }
        let api = Api {
            create: sym!(FCreate, b"sfizz_create_synth"),
            free: sym!(FPtr, b"sfizz_free"),
            load: sym!(FLoad, b"sfizz_load_file"),
            load_string: lib.get::<FLoadString>(b"sfizz_load_string").ok().map(|f| *f),
            set_sr: sym!(FSetF, b"sfizz_set_sample_rate"),
            set_block: sym!(FSetI, b"sfizz_set_samples_per_block"),
            set_voices: sym!(FSetI, b"sfizz_set_num_voices"),
            freewheel: sym!(FPtr, b"sfizz_enable_freewheeling"),
            num_regions: sym!(FGetI, b"sfizz_get_num_regions"),
            active_voices: sym!(FGetI, b"sfizz_get_num_active_voices"),
            note_on: sym!(FMsg3, b"sfizz_send_note_on"),
            note_off: sym!(FMsg3, b"sfizz_send_note_off"),
            cc: sym!(FMsg3, b"sfizz_send_cc"),
            pitch_wheel: sym!(FMsg2, b"sfizz_send_pitch_wheel"),
            render: sym!(FRender, b"sfizz_render_block"),
            all_sound_off: lib.get::<FPtr>(b"sfizz_all_sound_off").ok().map(|f| *f),
            set_tuning_frequency: lib.get::<FSetF>(b"sfizz_set_tuning_frequency").ok().map(|f| *f),
            _lib: lib,
        };
        Ok(api)
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ev {
    On(u8, u8),
    Off(u8),
    Cc(u8, u8),
    Bend(i16),
}

/// One sfizz synth instance (one per track per render).
pub struct Synth {
    api: &'static Api,
    h: *mut c_void,
    block: usize,
    pub num_regions: i32,
}

unsafe impl Send for Synth {}

impl Synth {
    pub fn new(sfz: &std::path::Path, sr: u32, block: usize) -> Result<Synth> {
        let api = api()?;
        let path = CString::new(sfz.to_string_lossy().as_bytes())?;
        // SAFETY: `h` is the handle `create` just made and `path` outlives the call
        Self::create(api, sr, block, |h| unsafe { (api.load)(h, path.as_ptr()) })
            .map_err(|()| anyhow!("sfizz could not load {}", sfz.display()))
    }

    /// Load SFZ text instead of a file. `virtual_path` stands for the file the text would be in: sample paths
    /// and `#include`s are resolved from its folder, as if the text had been read from there (the file itself
    /// need not exist).
    pub fn from_string(text: &str, virtual_path: &std::path::Path, sr: u32, block: usize) -> Result<Synth> {
        let api = api()?;
        let load_string = api.load_string.ok_or_else(|| anyhow!("this sfizz library has no sfizz_load_string"))?;
        let path = CString::new(virtual_path.to_string_lossy().as_bytes())?;
        let text = CString::new(text).map_err(|_| anyhow!("SFZ text for {} contains a NUL byte", virtual_path.display()))?;
        // SAFETY: `h` is the handle `create` just made; `path` and `text` outlive the call
        Self::create(api, sr, block, |h| unsafe { load_string(h, path.as_ptr(), text.as_ptr()) })
            .map_err(|()| anyhow!("sfizz could not load the text given for {}", virtual_path.display()))
    }

    /// A new synth set up for offline rendering, loaded by `load` (which returns false on failure).
    fn create(api: &'static Api, sr: u32, block: usize, load: impl FnOnce(*mut c_void) -> bool) -> std::result::Result<Synth, ()> {
        unsafe {
            let h = (api.create)();
            (api.set_sr)(h, sr as f32);
            (api.set_block)(h, block as c_int);
            (api.freewheel)(h);
            (api.set_voices)(h, 256);
            if !load(h) {
                (api.free)(h);
                return Err(());
            }
            let n = (api.num_regions)(h);
            Ok(Synth { api, h, block, num_regions: n })
        }
    }

    /// Render a sorted event list (sample time, event) into planar stereo buffers.
    pub fn render(&mut self, events: &[(u64, Ev)], frames: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0f32; frames];
        let mut r = vec![0f32; frames];
        let mut bl = vec![0f32; self.block];
        let mut br = vec![0f32; self.block];
        let mut ei = 0;
        let mut start = 0usize;
        while start < frames {
            let end = start + self.block;
            while ei < events.len() && (events[ei].0 as usize) < end {
                let (t, ev) = events[ei];
                let d = (t as usize).saturating_sub(start) as c_int;
                unsafe {
                    match ev {
                        Ev::On(n, v) => (self.api.note_on)(self.h, d, n as c_int, v as c_int),
                        Ev::Off(n) => (self.api.note_off)(self.h, d, n as c_int, 0),
                        Ev::Cc(c, v) => (self.api.cc)(self.h, d, c as c_int, v as c_int),
                        Ev::Bend(b) => (self.api.pitch_wheel)(self.h, d, b as c_int),
                    }
                }
                ei += 1;
            }
            bl.iter_mut().for_each(|x| *x = 0.0);
            br.iter_mut().for_each(|x| *x = 0.0);
            let mut ptrs = [bl.as_mut_ptr(), br.as_mut_ptr()];
            unsafe { (self.api.render)(self.h, ptrs.as_mut_ptr(), 2, self.block as c_int) };
            let n = self.block.min(frames - start);
            l[start..start + n].copy_from_slice(&bl[..n]);
            r[start..start + n].copy_from_slice(&br[..n]);
            start = end;
        }
        (l, r)
    }

    /// Queue one event `delay` frames into the next `render_block`.
    pub fn send(&mut self, delay: usize, ev: Ev) {
        let d = delay as c_int;
        unsafe {
            match ev {
                Ev::On(n, v) => (self.api.note_on)(self.h, d, n as c_int, v as c_int),
                Ev::Off(n) => (self.api.note_off)(self.h, d, n as c_int, 0),
                Ev::Cc(c, v) => (self.api.cc)(self.h, d, c as c_int, v as c_int),
                Ev::Bend(b) => (self.api.pitch_wheel)(self.h, d, b as c_int),
            }
        }
    }

    /// Render up to `block` frames (live playback), adding into nothing: `l` and `r` are overwritten.
    pub fn render_block(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len()).min(self.block);
        l[..n].iter_mut().for_each(|x| *x = 0.0);
        r[..n].iter_mut().for_each(|x| *x = 0.0);
        let mut ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        unsafe { (self.api.render)(self.h, ptrs.as_mut_ptr(), 2, n as c_int) };
    }

    /// Silence every voice at once (seek / stop).
    pub fn all_sound_off(&mut self) -> bool {
        match self.api.all_sound_off {
            Some(f) => {
                unsafe { f(self.h) };
                true
            }
            None => false,
        }
    }

    /// Tune the whole instrument: the frequency of A4 (440 Hz untuned).
    pub fn set_tuning_frequency(&mut self, hz: f32) -> Result<()> {
        let f = self.api.set_tuning_frequency.ok_or_else(|| anyhow!("this sfizz has no sfizz_set_tuning_frequency"))?;
        unsafe { f(self.h, hz) };
        Ok(())
    }

    pub fn block(&self) -> usize {
        self.block
    }

    pub fn active_voices(&self) -> i32 {
        unsafe { (self.api.active_voices)(self.h) }
    }
}

impl Drop for Synth {
    fn drop(&mut self) {
        unsafe { (self.api.free)(self.h) }
    }
}
