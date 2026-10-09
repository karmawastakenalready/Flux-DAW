//! Flux's side of out-of-process hosting: start a `flux-plugin-host`, control it, and stream audio
//! through shared memory.
//!
//! * [`PluginHost`]: the control connection (cheap to clone; any thread). Requests block until the
//!   host answers, the host exits, or a timeout passes.
//! * [`AudioPort`]: the audio connection, owned by whoever renders (an offline bounce or the live
//!   engine's audio callback). [`AudioPort::begin`] / [`AudioPort::end`] split a block so several
//!   plugin processes can run in parallel while the caller does other work.
//!
//! The host process is placed in a job object, so it dies with Flux even if Flux crashes.

#![cfg(windows)]

use crate::ipc::*;
use crate::plugin::{ParamPoint, PluginInfo, ProcessReport, ProgramList, Setup, TimedMidi, Transport};
use crate::sys::{self, Event, SharedMemory, Wait};
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Lines of the host's stderr kept for error messages.
const LOG_LINES: usize = 64;
/// Host events kept until someone reads them.
const MAX_EVENTS_KEPT: usize = 4096;

/// Where the host executable is and how long to wait for it.
#[derive(Clone, Debug)]
pub struct HostOptions {
    /// `flux-plugin-host.exe`; None = find it (see [`find_host_exe`]).
    pub exe: Option<PathBuf>,
    /// Arguments before the mode (`serve`), for executables that host plugins as a subcommand
    /// (Flux runs `flux.exe plugin-host serve`).
    pub args: Vec<String>,
    /// Ordinary requests (parameters, state, editor).
    pub timeout: Duration,
    /// Loading the plugin (big sample-based instruments can take minutes the first time).
    pub load_timeout: Duration,
    /// A name for messages ("Lead (Surge XT)").
    pub label: String,
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions { exe: None, args: vec![], timeout: Duration::from_secs(30), load_timeout: Duration::from_secs(300), label: "plugin".into() }
    }
}

/// The plugin host executable: `$FLUX_PLUGIN_HOST`, else next to the current executable (or one
/// directory up, for test binaries in `target/<profile>/deps`).
pub fn find_host_exe() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FLUX_PLUGIN_HOST") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!("FLUX_PLUGIN_HOST points to {}, which doesn't exist", p.display());
    }
    let exe = std::env::current_exe().context("locating the current executable")?;
    let name = "flux-plugin-host.exe";
    for dir in exe.ancestors().skip(1).take(2) {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("{name} wasn't found next to {} (build it with `cargo build -p flux-vst3`)", exe.display())
}

/// Receives host events on the reader thread.
pub type EventListener = Arc<dyn Fn(&HostEvent) + Send + Sync>;

/// What a running host reports when asked for parameters.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ParamState {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub short_title: String,
    #[serde(default)]
    pub units: String,
    pub steps: i32,
    pub default: f64,
    /// Normalized 0..1.
    pub value: f64,
    pub display: Option<String>,
    pub unit_id: i32,
    #[serde(default)]
    pub flags: Vec<String>,
    /// Display text of each step (stepped parameters, when asked for).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

impl ParamState {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
}

/// What a parameter was set to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ParamSet {
    pub name: String,
    pub id: u32,
    pub title: String,
    pub normalized: f64,
    pub display: Option<String>,
}

/// What a parameter value resolves to (see [`PluginHost::resolve_params`]).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ParamResolved {
    pub name: String,
    /// Whether the plugin lets a host change it during playback (VST3 `kCanAutomate`).
    #[serde(default)]
    pub automatable: bool,
    /// The parameter, its value normalized, and its value now; or why it didn't resolve.
    #[serde(flatten)]
    pub resolved: Resolved,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Resolved {
    Ok { id: u32, title: String, normalized: f64, current: f64 },
    Err { error: String },
}

/// A loaded plugin's description.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Loaded {
    pub info: PluginInfo,
    /// MIDI controllers (on channel 1) the plugin maps to parameters.
    pub midi_mapped: usize,
    pub program_lists: Vec<ProgramListSummary>,
    pub dirty: bool,
    #[serde(default)]
    pub editor_open: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProgramListSummary {
    pub id: i32,
    pub name: String,
    pub programs: usize,
}

/// How to load a plugin.
#[derive(Clone, Debug)]
pub struct LoadOptions {
    /// The `.vst3` bundle (or the binary inside it).
    pub path: PathBuf,
    /// Class id or name; None = the first audio processor.
    pub class: Option<String>,
    pub setup: Setup,
    /// Audio channels sent to the plugin's main input (0 for instruments).
    pub in_channels: u32,
    pub out_channels: u32,
    /// Capacity of one block's MIDI events / parameter points.
    pub max_events: u32,
    pub max_params: u32,
}

impl LoadOptions {
    pub fn instrument(path: impl Into<PathBuf>, setup: Setup) -> LoadOptions {
        LoadOptions { path: path.into(), class: None, setup, in_channels: 0, out_channels: 2, max_events: 2048, max_params: 512 }
    }

    pub fn effect(path: impl Into<PathBuf>, setup: Setup) -> LoadOptions {
        LoadOptions { in_channels: 2, ..LoadOptions::instrument(path, setup) }
    }
}

struct Reply {
    ok: bool,
    error: Option<String>,
    result: Value,
    blob: Vec<u8>,
}

/// State the reader threads update.
struct Shared {
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
    events: Mutex<VecDeque<HostEvent>>,
    listener: Mutex<Option<EventListener>>,
    /// Receivers of every event from when they asked ([`PluginHost::watch`]); dropped ones are forgotten.
    watchers: Mutex<Vec<mpsc::Sender<HostEvent>>>,
    log: Mutex<VecDeque<String>>,
    /// The control stream ended (the host exited or closed it).
    closed: AtomicBool,
}

struct Inner {
    shared: Arc<Shared>,
    child: Mutex<Child>,
    /// The process handle (owned by `child`), for waits.
    process: isize,
    pid: u32,
    stdin: Mutex<Option<ChildStdin>>,
    next_id: AtomicU64,
    opts: HostOptions,
}

impl Inner {
    fn exited(&self) -> Option<u32> {
        sys::exit_code(self.process as _)
    }

    /// Why the host is gone, with the end of its log.
    fn death(&self) -> anyhow::Error {
        // give the process a moment to finish exiting so the code is known
        // SAFETY: valid process handle owned by `child`.
        unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.process as _, 500) };
        let how = match self.exited() {
            Some(c) => describe_exit(c),
            None => "stopped responding".into(),
        };
        let log = self.shared.log.lock();
        let tail: Vec<&str> = log.iter().rev().take(8).rev().map(String::as_str).collect();
        if tail.is_empty() {
            anyhow!("the plugin host for {} {how}", self.opts.label)
        } else {
            anyhow!("the plugin host for {} {how}; its last output:\n{}", self.opts.label, tail.join("\n"))
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // ask nicely (the plugin gets to tear down), then insist
        if self.exited().is_none() && !self.shared.closed.load(Ordering::Acquire) {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if let Some(w) = self.stdin.lock().as_mut() {
                let _ = write_frame(w, &Envelope { id, req: Request::Shutdown }, &[]);
            }
        }
        // closing stdin also tells the host to quit
        self.stdin.lock().take();
        // SAFETY: valid process handle owned by `child`.
        let r = unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.process as _, 5000) };
        if r != windows_sys::Win32::Foundation::WAIT_OBJECT_0 {
            let _ = self.child.lock().kill();
        }
        let _ = self.child.lock().wait();
    }
}

/// The control connection to one plugin host process.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<Inner>,
}

impl PluginHost {
    /// Start a host process and check it speaks our protocol.
    pub fn spawn(opts: HostOptions) -> Result<PluginHost> {
        let exe = match &opts.exe {
            Some(e) => e.clone(),
            None => find_host_exe()?,
        };
        let mut child = Command::new(&exe)
            .args(&opts.args)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        let process = child.as_raw_handle() as isize;
        if let Some(job) = sys::job()
            && let Err(e) = job.assign(process as _)
        {
            // still usable; it just won't be killed automatically if Flux crashes
            eprintln!("[flux-vst3] warning: {e:#}");
        }
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take();
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(VecDeque::new()),
            listener: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            log: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
        });
        let s = shared.clone();
        std::thread::Builder::new().name("flux-vst3-reply".into()).spawn(move || read_replies(stdout, s))?;
        let s = shared.clone();
        std::thread::Builder::new().name("flux-vst3-log".into()).spawn(move || read_log(stderr, s))?;
        let pid = child.id();
        let host = PluginHost {
            inner: Arc::new(Inner { shared, child: Mutex::new(child), process, pid, stdin: Mutex::new(stdin), next_id: AtomicU64::new(1), opts }),
        };
        host.call(Request::Hello { version: PROTOCOL_VERSION }, &[], Duration::from_secs(20)).context("starting the plugin host")?;
        Ok(host)
    }

    /// Send a request and wait for its reply.
    fn call(&self, req: Request, blob: &[u8], timeout: Duration) -> Result<(Value, Vec<u8>)> {
        let inner = &self.inner;
        if inner.shared.closed.load(Ordering::Acquire) || inner.exited().is_some() {
            return Err(inner.death());
        }
        let op = op_name(&req);
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        inner.shared.pending.lock().insert(id, tx);
        let sent = {
            let mut w = inner.stdin.lock();
            match w.as_mut() {
                Some(w) => write_frame(w, &Envelope { id, req }, blob),
                None => Err(anyhow!("the connection is closed")),
            }
        };
        if sent.is_err() {
            inner.shared.pending.lock().remove(&id);
            return Err(inner.death());
        }
        match rx.recv_timeout(timeout) {
            Ok(r) if r.ok => Ok((r.result, r.blob)),
            Ok(r) => Err(anyhow!("{}: {}", inner.opts.label, r.error.unwrap_or_else(|| format!("{op} failed")))),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                inner.shared.pending.lock().remove(&id);
                bail!("the plugin host for {} didn't answer '{op}' within {:.0} s (the plugin may be hung)", inner.opts.label, timeout.as_secs_f64())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(inner.death()),
        }
    }

    fn call_json<T: serde::de::DeserializeOwned>(&self, req: Request, key: Option<&str>) -> Result<T> {
        let (v, _) = self.call(req, &[], self.inner.opts.timeout)?;
        let v = match key {
            Some(k) => v.get(k).cloned().unwrap_or(Value::Null),
            None => v,
        };
        serde_json::from_value(v).context("unexpected reply from the plugin host")
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    pub fn label(&self) -> &str {
        &self.inner.opts.label
    }

    /// The process is running and its control stream is open.
    pub fn alive(&self) -> bool {
        !self.inner.shared.closed.load(Ordering::Acquire) && self.inner.exited().is_none()
    }

    /// The last lines the host (and the plugin) wrote to stderr.
    pub fn log_tail(&self) -> Vec<String> {
        self.inner.shared.log.lock().iter().cloned().collect()
    }

    /// Load the plugin and connect its audio. One plugin per host.
    pub fn load(&self, o: &LoadOptions) -> Result<(Loaded, AudioPort)> {
        if o.in_channels > crate::server::MAX_CHANNELS || o.out_channels == 0 || o.out_channels > crate::server::MAX_CHANNELS {
            bail!("unsupported channel counts {} in / {} out", o.in_channels, o.out_channels);
        }
        let port = AudioPort::create(self.inner.clone(), o)?;
        let req = Request::Load {
            path: o.path.to_string_lossy().into_owned(),
            class: o.class.clone(),
            setup: o.setup,
            audio: port.names.clone(),
            in_channels: o.in_channels,
            out_channels: o.out_channels,
        };
        let (v, _) = self.call(req, &[], self.inner.opts.load_timeout).with_context(|| format!("loading {}", o.path.display()))?;
        let loaded: Loaded = serde_json::from_value(v).context("unexpected reply from the plugin host")?;
        Ok((loaded, port))
    }

    pub fn info(&self) -> Result<Loaded> {
        self.call_json(Request::Info, None)
    }

    /// Parameters with current values (filter: case-insensitive substring of the title).
    pub fn params(&self, filter: Option<&str>) -> Result<Vec<ParamState>> {
        self.call_json(Request::Params { filter: filter.map(String::from), choices: false }, Some("params"))
    }

    /// Like [`Self::params`], plus the display text of every step of stepped parameters (up to 128 steps).
    pub fn params_with_choices(&self, filter: Option<&str>) -> Result<Vec<ParamState>> {
        self.call_json(Request::Params { filter: filter.map(String::from), choices: true }, Some("params"))
    }

    /// Set parameters by name with human values (see [`crate::params`]). All or nothing.
    pub fn set_params(&self, values: &[(String, Value)]) -> Result<Vec<ParamSet>> {
        self.call_json(Request::SetParams { values: values.to_vec() }, Some("set"))
    }

    /// Resolve parameters by name with human values, as [`Self::set_params`] would, without setting them.
    pub fn resolve_params(&self, values: &[(String, Value)]) -> Result<Vec<ParamResolved>> {
        self.call_json(Request::ResolveParams { values: values.to_vec() }, Some("resolved"))
    }

    /// The plugin's state as `.vstpreset` bytes.
    pub fn state(&self) -> Result<Vec<u8>> {
        Ok(self.call(Request::GetState, &[], self.inner.opts.timeout)?.1)
    }

    /// Restore a `.vstpreset` (it must be for the loaded class).
    pub fn set_state(&self, preset: &[u8]) -> Result<()> {
        // big instruments load samples while restoring state
        self.call(Request::SetState, preset, self.inner.opts.load_timeout)?;
        Ok(())
    }

    pub fn programs(&self) -> Result<Vec<ProgramList>> {
        self.call_json(Request::Programs, Some("lists"))
    }

    /// Select a factory program; returns its name.
    pub fn set_program(&self, list: i32, index: usize) -> Result<String> {
        let (v, _) = self.call(Request::SetProgram { list, index }, &[], self.inner.opts.load_timeout)?;
        Ok(v.get("program").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    /// Show the plugin's editor (centered over `near`, a native window handle, if given).
    pub fn open_editor(&self, title: &str, near: Option<isize>) -> Result<()> {
        // let the host's window come to the front (Windows only allows that with the foreground app's consent)
        // SAFETY: plain call with a process id.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(self.inner.pid) };
        self.call(Request::OpenEditor { title: title.into(), parent: near.map(|h| h as i64) }, &[], self.inner.opts.timeout)?;
        Ok(())
    }

    pub fn close_editor(&self) -> Result<()> {
        self.call(Request::CloseEditor, &[], self.inner.opts.timeout)?;
        Ok(())
    }

    /// Clear voices, tails and delay lines (deactivate + reactivate).
    pub fn reset(&self) -> Result<()> {
        self.call(Request::Reset, &[], self.inner.opts.timeout)?;
        Ok(())
    }

    /// Events since the last call (parameter edits in the editor, latency changes, editor closed...).
    pub fn take_events(&self) -> Vec<HostEvent> {
        self.inner.shared.events.lock().drain(..).collect()
    }

    /// Also deliver events to `f` as they arrive (on the reader thread).
    pub fn set_listener(&self, f: Option<EventListener>) {
        *self.inner.shared.listener.lock() = f;
    }

    /// Every event from now on, for as long as the receiver is kept (the listener and `take_events` still get
    /// them too). Disconnects when the host stops.
    pub fn watch(&self) -> mpsc::Receiver<HostEvent> {
        let (tx, rx) = mpsc::channel();
        if !self.inner.shared.closed.load(Ordering::Acquire) {
            self.inner.shared.watchers.lock().push(tx);
        }
        rx
    }

    /// Stop the host now (the plugin tears down). Other clones of this handle stop working.
    pub fn shutdown(&self) -> Result<()> {
        let r = self.call(Request::Shutdown, &[], Duration::from_secs(10));
        self.inner.stdin.lock().take();
        // SAFETY: valid process handle owned by `child`.
        unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.inner.process as _, 10_000) };
        if self.inner.exited().is_none() {
            let _ = self.inner.child.lock().kill();
        }
        r.map(|_| ())
    }

    /// Kill the host without asking (e.g. after it stopped responding).
    pub fn kill(&self) {
        let _ = self.inner.child.lock().kill();
    }
}

fn op_name(r: &Request) -> String {
    serde_json::to_value(r).ok().and_then(|v| v.get("op").and_then(Value::as_str).map(String::from)).unwrap_or_else(|| "request".into())
}

fn read_replies(mut stdout: std::process::ChildStdout, shared: Arc<Shared>) {
    loop {
        match read_frame(&mut stdout) {
            Ok(Some((v, blob))) => match serde_json::from_value::<Message>(v) {
                Ok(Message::Reply { id, ok, error, result }) => {
                    if let Some(tx) = shared.pending.lock().remove(&id) {
                        let _ = tx.send(Reply { ok, error, result, blob });
                    }
                }
                Ok(Message::Event(e)) => {
                    if let Some(l) = shared.listener.lock().clone() {
                        l(&e);
                    }
                    shared.watchers.lock().retain(|w| w.send(e.clone()).is_ok());
                    let mut q = shared.events.lock();
                    if q.len() >= MAX_EVENTS_KEPT {
                        q.pop_front();
                    }
                    q.push_back(e);
                }
                Err(e) => push_log(&shared, format!("[flux-vst3] unreadable message from the host: {e}")),
            },
            Ok(None) => break,
            Err(e) => {
                push_log(&shared, format!("[flux-vst3] control stream failed: {e:#}"));
                break;
            }
        }
    }
    shared.closed.store(true, Ordering::Release);
    shared.watchers.lock().clear();
    // wake every waiter: their senders drop
    shared.pending.lock().clear();
}

fn push_log(shared: &Shared, line: String) {
    let mut l = shared.log.lock();
    if l.len() >= LOG_LINES {
        l.pop_front();
    }
    l.push_back(line);
}

fn read_log(stderr: std::process::ChildStderr, shared: Arc<Shared>) {
    let mut r = BufReader::new(stderr);
    let mut buf = vec![];
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                if !line.is_empty() {
                    push_log(&shared, line);
                }
            }
        }
    }
}

/// A process exit code in words.
pub fn describe_exit(code: u32) -> String {
    let what = match code {
        0 => return "exited".into(),
        0xC000_0005 => "an access violation",
        0xC000_00FD => "a stack overflow",
        0xC000_0409 => "a fail-fast / buffer overrun check",
        0xC000_0374 => "heap corruption",
        0xC000_001D => "an illegal instruction",
        0xC000_0094 => "an integer division by zero",
        0x8000_0003 => "a breakpoint",
        0xE06D_7363 => "an unhandled C++ exception",
        0xC000_013A => "Ctrl+C",
        1 => return "exited with code 1".into(),
        _ => return format!("exited with code {code} (0x{code:08X})"),
    };
    format!("crashed ({what}, 0x{code:08X})")
}

// ---- audio ----------------------------------------------------------------------------------------

/// Why a block didn't come back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioError {
    /// The host didn't finish the block in time (it may finish it later; the next block still works).
    Timeout,
    /// The host process is gone.
    Exited,
    /// The request was invalid (frames 0 or above max_block) or `end` came without `begin`.
    BadRequest,
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AudioError::Timeout => "the plugin didn't finish its audio block in time",
            AudioError::Exited => "the plugin host exited",
            AudioError::BadRequest => "invalid audio block request",
        })
    }
}

impl std::error::Error for AudioError {}

/// A block sent and not yet collected.
#[derive(Clone, Copy, Debug)]
struct Flight {
    frames: usize,
    /// Events / points that didn't fit the shared-memory lists.
    overflow: u32,
}

/// The audio connection to a loaded plugin. Realtime-safe: `begin`/`end`/`process` don't allocate or
/// take locks; they write shared memory and signal / wait on events.
pub struct AudioPort {
    inner: Arc<Inner>,
    names: AudioNames,
    shm: SharedMemory,
    request: Event,
    done: Event,
    layout: ShmLayout,
    setup: Setup,
    in_channels: usize,
    out_channels: usize,
    max_events: usize,
    max_params: usize,
    seq: u64,
    flight: Option<Flight>,
    timeout_ms: u32,
}

// SAFETY: the mapping and handles are owned; the port is used from one thread at a time.
unsafe impl Send for AudioPort {}

impl AudioPort {
    fn create(inner: Arc<Inner>, o: &LoadOptions) -> Result<AudioPort> {
        let (ic, oc, me, mp) = (o.in_channels as usize, o.out_channels as usize, o.max_events as usize, o.max_params as usize);
        let layout = ShmLayout::new(o.setup.max_block, ic, oc, me, mp);
        let names = AudioNames {
            shm: sys::unique_name("shm"),
            request: sys::unique_name("request"),
            done: sys::unique_name("done"),
            max_events: o.max_events,
            max_params: o.max_params,
        };
        let shm = SharedMemory::create(&names.shm, layout.size)?;
        // SAFETY: a fresh zeroed mapping of layout.size bytes; the header is at its start and nobody else uses it yet.
        unsafe {
            let h = shm.ptr() as *mut ShmHeader;
            (*h).magic = SHM_MAGIC;
            (*h).version = PROTOCOL_VERSION;
            (*h).max_block = o.setup.max_block as u32;
            (*h).in_channels = o.in_channels;
            (*h).out_channels = o.out_channels;
            (*h).max_events = o.max_events;
            (*h).max_params = o.max_params;
        }
        let request = Event::create(&names.request)?;
        let done = Event::create(&names.done)?;
        Ok(AudioPort {
            inner,
            names,
            shm,
            request,
            done,
            layout,
            setup: o.setup,
            in_channels: ic,
            out_channels: oc,
            max_events: me,
            max_params: mp,
            seq: 0,
            flight: None,
            // offline: the plugin may load samples in its first blocks; realtime: a few buffers at most
            timeout_ms: if o.setup.offline { 120_000 } else { 1000 },
        })
    }

    pub fn setup(&self) -> Setup {
        self.setup
    }

    pub fn out_channels(&self) -> usize {
        self.out_channels
    }

    pub fn in_channels(&self) -> usize {
        self.in_channels
    }

    /// How long `end` waits for a block.
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout_ms = t.as_millis().clamp(1, u32::MAX as u128 - 1) as u32;
    }

    /// The plugin's latency as of the last block.
    pub fn latency(&self) -> u32 {
        // SAFETY: an aligned u32 of the header, written by the host before it signals `done`.
        unsafe { std::ptr::read_volatile(&raw const (*(self.shm.ptr() as *const ShmHeader)).latency) }
    }

    /// Send a block of `frames` (<= max_block) and return without waiting. `inputs`: one slice per
    /// input channel (ignored if the plugin was loaded without inputs). `midi` and `params` are
    /// sorted by offset, relative to the block; past the capacity they're dropped (and counted).
    pub fn begin(&mut self, frames: usize, inputs: &[&[f32]], midi: &[TimedMidi], params: &[ParamPoint], transport: Option<&Transport>) -> Result<(), AudioError> {
        self.send(frames, inputs, 0, midi, params, transport)
    }

    /// `begin` for the piece of a longer buffer starting at `at`: inputs are read from `at`, and
    /// events/points carry offsets relative to the whole buffer (the caller passes only those inside
    /// the piece).
    fn send(&mut self, frames: usize, inputs: &[&[f32]], at: usize, midi: &[TimedMidi], params: &[ParamPoint], transport: Option<&Transport>) -> Result<(), AudioError> {
        if frames == 0 || frames > self.setup.max_block {
            return Err(AudioError::BadRequest);
        }
        // a block that was never collected is abandoned (its sequence number won't match)
        self.flight = None;
        let base = self.shm.ptr();
        let max_block = self.setup.max_block;
        let rebase = at as u32;
        // SAFETY: every write is inside the mapping (ShmLayout); counts are clamped to the capacities.
        // The host reads these regions only between our `request` signal and its `done` signal.
        unsafe {
            let h = base as *mut ShmHeader;
            let ne = midi.len().min(self.max_events);
            let ev = base.add(self.layout.events) as *mut ShmMidi;
            for (i, e) in midi[..ne].iter().enumerate() {
                let rel = TimedMidi { offset: e.offset.saturating_sub(rebase), msg: e.msg };
                std::ptr::write_volatile(ev.add(i), ShmMidi::from(&rel));
            }
            let np = params.len().min(self.max_params);
            let pp = base.add(self.layout.params) as *mut ShmParam;
            for (i, p) in params[..np].iter().enumerate() {
                let rel = ParamPoint { offset: p.offset.saturating_sub(rebase), ..*p };
                std::ptr::write_volatile(pp.add(i), ShmParam::from(&rel));
            }
            let with_input = self.in_channels > 0 && !inputs.is_empty();
            if with_input {
                let ip = base.add(self.layout.input) as *mut f32;
                for c in 0..self.in_channels {
                    let dst = std::slice::from_raw_parts_mut(ip.add(c * max_block), frames);
                    // fewer inputs than channels: repeat the last (mono into a stereo effect)
                    let src = inputs[c.min(inputs.len() - 1)];
                    let avail = src.len().saturating_sub(at).min(frames);
                    dst[..avail].copy_from_slice(&src[at..at + avail]);
                    dst[avail..].fill(0.0);
                }
            }
            std::ptr::write_volatile(&raw mut (*h).frames, frames as u32);
            std::ptr::write_volatile(&raw mut (*h).n_events, ne as u32);
            std::ptr::write_volatile(&raw mut (*h).n_params, np as u32);
            std::ptr::write_volatile(&raw mut (*h).flags, with_input as u32);
            std::ptr::write_volatile(&raw mut (*h).transport, ShmTransport::from(transport));
            self.seq += 1;
            (*h).seq.store(self.seq, Ordering::Release);
            self.flight = Some(Flight { frames, overflow: (midi.len() - ne + params.len() - np) as u32 });
        }
        self.request.set();
        Ok(())
    }

    /// The host has finished the last block sent (or none was sent). Never waits: for checking on a
    /// plugin that missed its deadline, since until it is done, sending another block would overwrite
    /// shared memory it may still be reading.
    pub fn idle(&self) -> bool {
        // SAFETY: atomic field of the header.
        unsafe { (*(self.shm.ptr() as *const ShmHeader)).done_seq.load(Ordering::Acquire) == self.seq }
    }

    /// Wait for the block sent by `begin` and copy its output into `outputs` (one slice per channel;
    /// extra channels repeat the plugin's last one). On error the outputs are silenced.
    pub fn end(&mut self, outputs: &mut [&mut [f32]]) -> Result<ProcessReport, AudioError> {
        self.collect(outputs, 0)
    }

    fn collect(&mut self, outputs: &mut [&mut [f32]], at: usize) -> Result<ProcessReport, AudioError> {
        let Some(flight) = self.flight.take() else {
            silence(outputs, at, usize::MAX);
            return Err(AudioError::BadRequest);
        };
        let frames = flight.frames;
        let h = self.shm.ptr() as *const ShmHeader;
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms as u64);
        loop {
            // SAFETY: atomic field of the header.
            if unsafe { (*h).done_seq.load(Ordering::Acquire) } == self.seq {
                break;
            }
            // `done` may have been set by an abandoned block: then the check above fails and we wait again
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = if left.is_zero() { Wait::Timeout } else { self.done.wait_or_exit(self.inner.process as _, left.as_millis().max(1) as u32) };
            match wait {
                Wait::Signaled => {}
                Wait::Timeout => {
                    silence(outputs, at, frames);
                    return Err(AudioError::Timeout);
                }
                Wait::ProcessExited | Wait::Failed => {
                    silence(outputs, at, frames);
                    return Err(AudioError::Exited);
                }
            }
        }
        // SAFETY: the host finished this block: the response fields and the output region are ours to
        // read until the next `begin`.
        unsafe {
            if std::ptr::read_volatile(&raw const (*h).status) != 0 {
                silence(outputs, at, frames);
                return Err(AudioError::BadRequest);
            }
            let op = self.shm.ptr().add(self.layout.output) as *const f32;
            for (c, out) in outputs.iter_mut().enumerate() {
                if out.len() <= at {
                    continue;
                }
                let src = std::slice::from_raw_parts(op.add(c.min(self.out_channels - 1) * self.setup.max_block), frames);
                let n = (out.len() - at).min(frames);
                out[at..at + n].copy_from_slice(&src[..n]);
            }
            Ok(ProcessReport {
                skipped: std::ptr::read_volatile(&raw const (*h).skipped) != 0,
                unmapped: std::ptr::read_volatile(&raw const (*h).unmapped),
                dropped: std::ptr::read_volatile(&raw const (*h).dropped) + flight.overflow,
            })
        }
    }

    /// Process `frames` of any length: split into max_block pieces, each event / point routed to the
    /// piece it falls in. `midi` and `params` are sorted, with offsets relative to the whole buffer.
    pub fn process(
        &mut self,
        frames: usize,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi: &[TimedMidi],
        params: &[ParamPoint],
        transport: Option<&Transport>,
    ) -> Result<ProcessReport, AudioError> {
        let mut report = ProcessReport::default();
        let mut done = 0;
        let (mut mi, mut pi) = (0, 0);
        while done < frames {
            let n = (frames - done).min(self.setup.max_block);
            let end = (done + n) as u32;
            let m_end = mi + midi[mi..].iter().take_while(|e| e.offset < end).count();
            let p_end = pi + params[pi..].iter().take_while(|e| e.offset < end).count();
            let t = transport.map(|t| crate::plugin::advance(t, done, self.setup.sample_rate));
            self.send(n, inputs, done, &midi[mi..m_end], &params[pi..p_end], t.as_ref())?;
            let r = self.collect(outputs, done)?;
            report.skipped |= r.skipped;
            report.unmapped += r.unmapped;
            report.dropped += r.dropped;
            mi = m_end;
            pi = p_end;
            done += n;
        }
        Ok(report)
    }

    /// The host process id (for diagnostics).
    pub fn host_pid(&self) -> u32 {
        self.inner.pid
    }
}

/// Zero `n` frames of every output from `at`.
fn silence(outputs: &mut [&mut [f32]], at: usize, n: usize) {
    for o in outputs.iter_mut() {
        if o.len() > at {
            let e = at.saturating_add(n).min(o.len());
            o[at..e].fill(0.0);
        }
    }
}

/// Spawn a host and load a plugin in it.
pub fn open(opts: HostOptions, load: &LoadOptions) -> Result<(PluginHost, Loaded, AudioPort)> {
    let host = PluginHost::spawn(opts)?;
    let (loaded, port) = host.load(load)?;
    Ok((host, loaded, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_read_well() {
        assert_eq!(describe_exit(0), "exited");
        assert!(describe_exit(0xC000_0005).contains("access violation"));
        assert!(describe_exit(7).contains("code 7"));
    }

    #[test]
    fn silence_respects_ranges() {
        let mut a = vec![1.0f32; 8];
        let mut b = vec![1.0f32; 3];
        silence(&mut [&mut a[..], &mut b[..]], 2, 4);
        assert_eq!(a, [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0]);
        assert_eq!(b, [1.0, 1.0, 0.0]);
    }
}
