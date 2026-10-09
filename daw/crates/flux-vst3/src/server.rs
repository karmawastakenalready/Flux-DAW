//! The plugin host process (`flux-plugin-host`): one plugin instance per process, so a plugin that
//! crashes or hangs only takes its own process down.
//!
//! Threads:
//! * main (UI) thread: a Win32 message loop. VST3 wants controller and editor calls on the UI
//!   thread, and plugin editors need the loop to run. Requests are handled here, posted to a
//!   message-only window by the reader thread; a timer applies restart requests and forwards events.
//! * reader thread: reads request frames from stdin and queues them.
//! * audio thread: waits on the shared-memory channel and runs `Processor::process` per block.
//!
//! Modes:
//! * `flux-plugin-host serve`: the protocol in [`crate::ipc`] over stdin / stdout.
//! * `flux-plugin-host scan <path> [--deep]`: print one frame describing the module's classes
//!   (with `--deep`, instantiate every processor class and report its buses and parameters).

#![cfg(windows)]

use crate::editor::Editor;
use crate::ipc::*;
use crate::module::Module;
use crate::params;
use crate::plugin::{Plugin, Processor, Setup, TimedMidi};
use crate::sys::{self, Event, SharedMemory, Wait};
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use vst3::Steinberg::Vst::RestartFlags_::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::com::HandlerEvent;

/// Most audio channels per direction the shared-memory channel carries.
pub const MAX_CHANNELS: u32 = 8;
const WM_REQUEST: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;
const TICK_MS: u32 = 30;
/// Handler events kept between ticks (a plugin flooding performEdit can't grow memory without bound).
const MAX_QUEUED_EVENTS: usize = 65536;

/// Entry point of the `flux-plugin-host` binary.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args)
}

/// Run a plugin host with these arguments (`serve`, `scan <path> [--deep]`, `--version`); for
/// executables that host plugins as a subcommand.
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("serve") => serve(),
        Some("scan") if args.len() >= 2 => scan(Path::new(&args[1]), args.iter().any(|a| a == "--deep")),
        Some("--version") => {
            println!("flux-plugin-host {} (protocol {PROTOCOL_VERSION})", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: flux-plugin-host serve | scan <plugin.vst3> [--deep] | --version");
            eprintln!("(started by Flux; it isn't meant to be run by hand)");
            ExitCode::from(2)
        }
    }
}

/// Process-wide setup shared by both modes: no crash dialogs, a private protocol stdout, COM/OLE, DPI.
fn init() -> Result<std::fs::File> {
    sys::quiet_crashes();
    let out = sys::take_stdout()?;
    // SAFETY: plain process-wide initialisation calls on the main thread.
    unsafe {
        // plugin editors use OLE (drag and drop, clipboard); it also initialises COM as an STA
        OleInitialize(std::ptr::null());
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    Ok(out)
}

// ---- scan -----------------------------------------------------------------------------------------

fn scan(path: &Path, deep: bool) -> ExitCode {
    let mut out = match init() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("flux-plugin-host: {e:#}");
            return ExitCode::from(2);
        }
    };
    let r = scan_module(path, deep);
    let msg = match r {
        Ok(v) => Message::Reply { id: 0, ok: true, error: None, result: v },
        Err(e) => Message::Reply { id: 0, ok: false, error: Some(format!("{e:#}")), result: Value::Null },
    };
    let code = if write_frame(&mut out, &msg, &[]).is_ok() { ExitCode::SUCCESS } else { ExitCode::from(3) };
    // SAFETY: matches OleInitialize in init.
    unsafe { OleUninitialize() };
    code
}

fn scan_module(path: &Path, deep: bool) -> Result<Value> {
    let module = Arc::new(Module::load(path)?);
    let classes = module.classes()?;
    let mut list = vec![];
    for c in &classes {
        let mut v = serde_json::to_value(c)?;
        if deep && c.is_processor() {
            let setup = Setup { sample_rate: 48000.0, max_block: 512, offline: true };
            match Plugin::new(module.clone(), Some(&c.cid), setup) {
                Ok(p) => {
                    v["instance"] = json!({
                        "info": p.info(),
                        "midi_mapped": p.midi_map().mapped(),
                        "program_lists": p.program_lists().iter().map(|l| json!({"name": l.name, "programs": l.programs.len()})).collect::<Vec<_>>(),
                        "has_editor": p.create_view().is_some(),
                    });
                }
                Err(e) => v["error"] = json!(format!("{e:#}")),
            }
        }
        list.push(v);
    }
    Ok(json!({
        "bundle": module.paths.bundle,
        "binary": module.paths.binary,
        "factory": module.factory_info(),
        "classes": list,
    }))
}

// ---- serve ----------------------------------------------------------------------------------------

enum Incoming {
    Request(u64, Request, Vec<u8>),
    /// A frame that isn't a valid request: reply with this error.
    Bad(u64, String),
    /// stdin closed: Flux went away.
    Eof,
}

#[derive(Default)]
struct Queue {
    items: Mutex<VecDeque<Incoming>>,
}

thread_local! {
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };
}

fn serve() -> ExitCode {
    let out = match init() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("flux-plugin-host: {e:#}");
            return ExitCode::from(2);
        }
    };
    let hwnd = match message_window() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("flux-plugin-host: {e:#}");
            return ExitCode::from(2);
        }
    };
    let queue = Arc::new(Queue::default());
    let q = queue.clone();
    let hw = hwnd as isize;
    if let Err(e) = std::thread::Builder::new().name("flux-reader".into()).spawn(move || reader(q, hw)) {
        eprintln!("flux-plugin-host: starting the reader thread: {e}");
        return ExitCode::from(2);
    }
    SERVER.with(|s| *s.borrow_mut() = Some(Server { out, queue, loaded: None, quitting: false }));
    // SAFETY: standard message loop on the thread that owns the windows.
    unsafe {
        SetTimer(hwnd, TIMER_ID, TICK_MS, None);
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 || r == -1 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        KillTimer(hwnd, TIMER_ID);
    }
    // orderly teardown: audio thread, editor, plugin, module
    let server = SERVER.with(|s| s.borrow_mut().take());
    drop(server);
    // SAFETY: our own window; matches OleInitialize in init.
    unsafe {
        DestroyWindow(hwnd);
        OleUninitialize();
    }
    ExitCode::SUCCESS
}

fn message_window() -> Result<HWND> {
    let class = crate::util::wide("FluxPluginHostMessages");
    // SAFETY: registering a class with a valid window procedure and creating a message-only window.
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(message_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        if RegisterClassExW(&wc) == 0 {
            bail!("RegisterClassExW failed: {}", std::io::Error::last_os_error());
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            bail!("creating the message window failed: {}", std::io::Error::last_os_error());
        }
        std::mem::forget(class);
        Ok(hwnd)
    }
}

/// Runs `f` on the server unless it is already busy further up the stack (a plugin pumping messages
/// inside a call we made): the outer call drains the queue when it returns.
fn with_server(f: impl FnOnce(&mut Server)) {
    SERVER.with(|s| {
        if let Ok(mut g) = s.try_borrow_mut()
            && let Some(server) = g.as_mut()
        {
            f(server)
        }
    });
}

unsafe extern "system" fn message_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_REQUEST => {
            with_server(Server::pump);
            0
        }
        WM_TIMER if wp == TIMER_ID => {
            with_server(|s| {
                s.tick();
                s.pump();
            });
            0
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn reader(queue: Arc<Queue>, hwnd: isize) {
    let mut stdin = std::io::stdin().lock();
    loop {
        let inc = match read_frame(&mut stdin) {
            Ok(Some((v, blob))) => {
                let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
                match serde_json::from_value::<Envelope>(v) {
                    Ok(e) => Incoming::Request(e.id, e.req, blob),
                    Err(e) => Incoming::Bad(id, format!("bad request: {e}")),
                }
            }
            Ok(None) => Incoming::Eof,
            Err(e) => {
                eprintln!("flux-plugin-host: reading requests: {e:#}");
                Incoming::Eof
            }
        };
        let eof = matches!(inc, Incoming::Eof);
        queue.items.lock().push_back(inc);
        // SAFETY: posting to our message window (if it's gone, the post fails harmlessly).
        unsafe { PostMessageW(hwnd as HWND, WM_REQUEST, 0, 0) };
        if eof {
            break;
        }
    }
}

struct Server {
    out: std::fs::File,
    queue: Arc<Queue>,
    loaded: Option<Loaded>,
    quitting: bool,
}

/// A loaded plugin. Field order is teardown order: editor, audio thread, then the plugin.
struct Loaded {
    editor: Option<Editor>,
    audio: AudioThread,
    plugin: Plugin,
    events: Arc<Mutex<Vec<HandlerEvent>>>,
    last_dirty: bool,
    title: String,
}

impl Loaded {
    /// Let the processor take queued parameter changes (see [`Flush`]).
    fn flush(&self) -> Result<()> {
        if !self.audio.flush(std::time::Duration::from_secs(10)) {
            bail!("the plugin's audio processing didn't respond within 10 s");
        }
        Ok(())
    }

    /// Apply a change that should alter the plugin's state (a program change) and make it stick: some
    /// plugins ignore program changes for a moment after their state was restored (Dexed, for about
    /// 2 s, so a host's program call can't overwrite a recalled voice), so until the state changes,
    /// apply it again, for up to 3 s.
    fn apply_until_changed(&self, apply: impl Fn() -> Result<()>) -> Result<()> {
        let before = self.plugin.state().ok();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            apply()?;
            // the processor takes the change when it next processes
            self.flush()?;
            if before.is_none() || self.plugin.state().ok() != before || std::time::Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
}

impl Server {
    fn quit(&mut self) {
        if !self.quitting {
            self.quitting = true;
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
    }

    fn send(&mut self, msg: &Message, blob: &[u8]) {
        if let Err(e) = write_frame(&mut self.out, msg, blob) {
            // Flux is gone
            eprintln!("flux-plugin-host: writing to Flux: {e:#}");
            self.quit();
        }
    }

    fn event(&mut self, e: HostEvent) {
        self.send(&Message::Event(e), &[]);
    }

    fn reply(&mut self, id: u64, r: Result<(Value, Vec<u8>)>) {
        match r {
            Ok((result, blob)) => self.send(&Message::Reply { id, ok: true, error: None, result }, &blob),
            Err(e) => self.send(&Message::Reply { id, ok: false, error: Some(format!("{e:#}")), result: Value::Null }, &[]),
        }
    }

    /// Handle every queued request.
    fn pump(&mut self) {
        while !self.quitting {
            let Some(inc) = self.queue.items.lock().pop_front() else { break };
            match inc {
                Incoming::Eof => self.quit(),
                Incoming::Bad(id, e) => self.reply(id, Err(anyhow!(e))),
                Incoming::Request(id, req, blob) => {
                    let shutdown = matches!(req, Request::Shutdown);
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(req, blob))) {
                        Ok(r) => self.reply(id, r),
                        Err(p) => {
                            let what = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                            self.reply(id, Err(anyhow!("the plugin host failed internally ({what}) and is exiting")));
                            self.quit();
                        }
                    }
                    if shutdown {
                        self.quit();
                    }
                }
            }
        }
    }

    fn loaded(&mut self) -> Result<&mut Loaded> {
        self.loaded.as_mut().ok_or_else(|| anyhow!("no plugin is loaded in this host"))
    }

    fn handle(&mut self, req: Request, blob: Vec<u8>) -> Result<(Value, Vec<u8>)> {
        let none = Vec::<u8>::new;
        match req {
            Request::Hello { version } => {
                if version != PROTOCOL_VERSION {
                    bail!("protocol mismatch: Flux speaks version {version}, this plugin host {PROTOCOL_VERSION} (reinstall Flux)");
                }
                Ok((json!({"protocol": PROTOCOL_VERSION, "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id()}), none()))
            }
            Request::Load { path, class, setup, audio, in_channels, out_channels } => {
                if self.loaded.is_some() {
                    bail!("a plugin is already loaded in this host");
                }
                let loaded = load(&path, class.as_deref(), setup, &audio, in_channels, out_channels)?;
                let r = describe(&loaded.plugin);
                self.loaded = Some(loaded);
                Ok((r, none()))
            }
            Request::Info => {
                let l = self.loaded()?;
                let mut r = describe(&l.plugin);
                r["editor_open"] = json!(l.editor.is_some());
                Ok((r, none()))
            }
            Request::Params { filter, choices } => {
                let l = self.loaded()?;
                let f = filter.map(|f| f.to_lowercase());
                let list: Vec<Value> = l
                    .plugin
                    .params()
                    .into_iter()
                    .filter(|p| f.as_ref().is_none_or(|f| p.title.to_lowercase().contains(f) || p.short_title.to_lowercase().contains(f)))
                    .map(|p| {
                        let value = l.plugin.param(p.id).unwrap_or(p.default);
                        let mut flags = vec![];
                        for (on, name) in [
                            (p.automatable(), "automatable"),
                            (p.read_only(), "read_only"),
                            (p.hidden(), "hidden"),
                            (p.is_list(), "list"),
                            (p.program_change(), "program_change"),
                            (p.bypass(), "bypass"),
                        ] {
                            if on {
                                flags.push(name);
                            }
                        }
                        let mut j = json!({
                            "id": p.id,
                            "title": p.title,
                            "short_title": p.short_title,
                            "units": p.units,
                            "steps": p.step_count,
                            "default": p.default,
                            "value": value,
                            "display": l.plugin.param_text(p.id, value),
                            "unit_id": p.unit_id,
                            "flags": flags,
                        });
                        if choices && (1..=128).contains(&p.step_count) {
                            let n = p.step_count as f64;
                            let texts: Vec<String> =
                                (0..=p.step_count).map(|i| l.plugin.param_text(p.id, i as f64 / n).unwrap_or_default()).collect();
                            j["choices"] = json!(texts);
                        }
                        j
                    })
                    .collect();
                Ok((json!({"params": list}), none()))
            }
            Request::SetParams { values } => {
                let l = self.loaded()?;
                let all = l.plugin.params();
                // resolve everything first so a typo changes nothing
                let mut resolved = vec![];
                let mut errors = vec![];
                for (name, v) in &values {
                    match params::find(&all, name).and_then(|p| {
                        if p.read_only() {
                            bail!("parameter '{}' is read-only", p.title);
                        }
                        params::normalized(&l.plugin, p, v).map(|n| (p, n))
                    }) {
                        Ok((p, n)) => resolved.push((name, p, n)),
                        Err(e) => errors.push(format!("{e:#}")),
                    }
                }
                if !errors.is_empty() {
                    bail!("{}", errors.join("; "));
                }
                // a program change has to stick (see `apply_until_changed`)
                let program = resolved.iter().any(|(_, p, n)| p.program_change() && l.plugin.param(p.id).is_none_or(|v| (v - n).abs() > 1e-9));
                let apply = || {
                    for (_, p, n) in &resolved {
                        l.plugin.set_param(p.id, *n);
                    }
                    Ok(())
                };
                if program {
                    l.apply_until_changed(apply)?;
                } else {
                    apply()?;
                }
                let out: Vec<Value> = resolved
                    .iter()
                    .map(|(name, p, n)| {
                        let now = l.plugin.param(p.id).unwrap_or(*n);
                        json!({"name": name, "id": p.id, "title": p.title, "normalized": now, "display": l.plugin.param_text(p.id, now)})
                    })
                    .collect();
                Ok((json!({"set": out}), none()))
            }
            Request::ResolveParams { values } => {
                let l = self.loaded()?;
                let all = l.plugin.params();
                let out: Vec<Value> = values
                    .iter()
                    .map(|(name, v)| match params::find(&all, name).and_then(|p| params::normalized(&l.plugin, p, v).map(|n| (p, n))) {
                        Ok((p, n)) => json!({
                            "name": name, "id": p.id, "title": p.title, "normalized": n,
                            "current": l.plugin.param(p.id).unwrap_or(p.default), "automatable": p.automatable() && !p.read_only(),
                        }),
                        Err(e) => json!({"name": name, "error": format!("{e:#}")}),
                    })
                    .collect();
                Ok((json!({"resolved": out}), none()))
            }
            Request::GetState => {
                let l = self.loaded()?;
                if l.plugin.has_pending_params() {
                    l.flush()?;
                }
                let state = l.plugin.state()?;
                let preset = crate::preset::Preset { class_id: l.plugin.class().cid.clone(), state, info: None };
                let bytes = preset.to_bytes()?;
                l.last_dirty = false;
                Ok((json!({"bytes": bytes.len(), "component": preset.state.component.len(), "controller": preset.state.controller.len()}), bytes))
            }
            Request::SetState => {
                let l = self.loaded()?;
                let preset = crate::preset::Preset::from_bytes(&blob)?;
                if !preset.class_id.eq_ignore_ascii_case(&l.plugin.class().cid) {
                    bail!("this state belongs to class {} but the loaded plugin is {} ({})", preset.class_id, l.plugin.class().cid, l.plugin.class().name);
                }
                l.plugin.set_state(&preset.state)?;
                l.last_dirty = false;
                Ok((json!({"ok": true}), none()))
            }
            Request::Programs => {
                let l = self.loaded()?;
                Ok((json!({"lists": l.plugin.program_lists()}), none()))
            }
            Request::SetProgram { list, index } => {
                let l = self.loaded()?;
                let lists = l.plugin.program_lists();
                let pl = lists.iter().find(|p| p.id == list).ok_or_else(|| anyhow!("no program list with id {list}"))?;
                let target = if pl.programs.len() > 1 { index as f64 / (pl.programs.len() - 1) as f64 } else { 0.0 };
                let already = pl.param.and_then(|id| l.plugin.param(id)).is_some_and(|v| (v - target).abs() < 1e-9);
                if already {
                    l.plugin.set_program(pl, index)?;
                    l.flush()?;
                } else {
                    l.apply_until_changed(|| l.plugin.set_program(pl, index))?;
                }
                Ok((json!({"list": pl.name, "program": pl.programs[index]}), none()))
            }
            Request::OpenEditor { title, parent } => {
                let l = self.loaded()?;
                if let Some(e) = &l.editor {
                    e.focus();
                    return Ok((json!({"hwnd": e.hwnd(), "already_open": true}), none()));
                }
                let title = if title.is_empty() { l.plugin.class().name.clone() } else { title };
                let e = Editor::open(&l.plugin, &title, parent.map(|p| p as isize))?;
                let hwnd = e.hwnd();
                l.editor = Some(e);
                l.title = title;
                self.event(HostEvent::EditorOpened);
                Ok((json!({"hwnd": hwnd}), none()))
            }
            Request::CloseEditor => {
                let l = self.loaded()?;
                let was = l.editor.take().is_some();
                if was {
                    self.event(HostEvent::EditorClosed);
                }
                Ok((json!({"closed": was}), none()))
            }
            Request::Reset => {
                self.loaded()?.plugin.reset()?;
                Ok((json!({"ok": true}), none()))
            }
            Request::Shutdown => Ok((json!({"ok": true}), none())),
        }
    }

    /// Periodic UI-thread work: restart requests, values the processor reported, editor state, events.
    fn tick(&mut self) {
        if self.quitting {
            return;
        }
        let Some(l) = self.loaded.as_mut() else { return };
        let mut out = vec![];
        if l.editor.as_ref().is_some_and(Editor::closed) {
            l.editor = None;
            out.push(HostEvent::EditorClosed);
        }
        let flags = l.plugin.handle_restart();
        if flags & kLatencyChanged as i32 != 0 {
            out.push(HostEvent::Latency { samples: l.plugin.latency() });
        }
        if flags & (kParamTitlesChanged | kParamValuesChanged | kIoChanged | kReloadComponent) as i32 != 0 {
            out.push(HostEvent::ParamsChanged);
        }
        l.plugin.sync_reported();
        let mut open_editor = false;
        let events = std::mem::take(&mut *l.events.lock());
        let mut edits: Vec<(u32, f64)> = vec![];
        for e in events {
            match e {
                HandlerEvent::PerformEdit(id, v) => match edits.iter_mut().find(|x| x.0 == id) {
                    Some(x) => x.1 = v,
                    None => edits.push((id, v)),
                },
                HandlerEvent::RequestOpenEditor => open_editor = true,
                _ => {}
            }
        }
        out.extend(edits.into_iter().map(|(id, value)| HostEvent::ParamEdit { id, value }));
        let dirty = l.plugin.dirty();
        if dirty && !l.last_dirty {
            out.push(HostEvent::Dirty);
        }
        l.last_dirty = dirty;
        if open_editor && l.editor.is_none() {
            match Editor::open(&l.plugin, &l.title, None) {
                Ok(e) => {
                    l.editor = Some(e);
                    out.push(HostEvent::EditorOpened);
                }
                Err(e) => out.push(HostEvent::Log { message: format!("the plugin asked for its editor, which failed to open: {e:#}") }),
            }
        }
        for e in out {
            self.event(e);
        }
    }
}

fn describe(p: &Plugin) -> Value {
    json!({
        "info": p.info(),
        "midi_mapped": p.midi_map().mapped(),
        "program_lists": p.program_lists().iter().map(|l| json!({"id": l.id, "name": l.name, "programs": l.programs.len()})).collect::<Vec<_>>(),
        "dirty": p.dirty(),
    })
}

fn load(path: &str, class: Option<&str>, setup: Setup, names: &AudioNames, in_channels: u32, out_channels: u32) -> Result<Loaded> {
    if in_channels > MAX_CHANNELS || out_channels == 0 || out_channels > MAX_CHANNELS {
        bail!("unsupported channel counts {in_channels} in / {out_channels} out (at most {MAX_CHANNELS})");
    }
    if names.max_events as usize > 1 << 16 || names.max_params as usize > 1 << 16 {
        bail!("event or parameter capacity too large");
    }
    // the audio channel first: it's cheap to check and a mismatch means Flux and this host disagree
    let layout = ShmLayout::new(setup.max_block, in_channels as usize, out_channels as usize, names.max_events as usize, names.max_params as usize);
    let shm = SharedMemory::open(&names.shm, layout.size).context("opening the audio channel")?;
    // SAFETY: the mapping is at least `layout.size` bytes, which starts with a header.
    let header_ok = unsafe {
        let h = shm.ptr() as *const ShmHeader;
        (*h).magic == SHM_MAGIC
            && (*h).version == PROTOCOL_VERSION
            && (*h).max_block as usize == setup.max_block
            && (*h).in_channels == in_channels
            && (*h).out_channels == out_channels
            && (*h).max_events == names.max_events
            && (*h).max_params == names.max_params
    };
    if !header_ok {
        bail!("the audio channel's header doesn't match the load request");
    }
    let request = Arc::new(Event::open(&names.request)?);
    let done = Event::open(&names.done)?;

    let module = Arc::new(Module::load(Path::new(path))?);
    let mut plugin = Plugin::new(module, class, setup)?;
    let events: Arc<Mutex<Vec<HandlerEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    plugin.set_listener(Some(Arc::new(move |e| {
        let mut q = ev.lock();
        if q.len() < MAX_QUEUED_EVENTS {
            q.push(e);
        }
    })));
    let processor = plugin.processor()?;
    let channel = Channel { shm, request, done, layout, in_channels: in_channels as usize, out_channels: out_channels as usize, max_events: names.max_events as usize, max_params: names.max_params as usize };
    let audio = AudioThread::start(processor, channel, !setup.offline)?;
    let title = plugin.class().name.clone();
    let last_dirty = plugin.dirty();
    Ok(Loaded { editor: None, audio, plugin, events, last_dirty, title })
}

// ---- audio thread ---------------------------------------------------------------------------------

struct Channel {
    shm: SharedMemory,
    request: Arc<Event>,
    done: Event,
    layout: ShmLayout,
    in_channels: usize,
    out_channels: usize,
    max_events: usize,
    max_params: usize,
}

struct AudioThread {
    stop: Arc<AtomicBool>,
    wake: Arc<Event>,
    flush: Arc<Flush>,
    handle: Option<JoinHandle<()>>,
}

/// Frames in a flush block.
const FLUSH_FRAMES: usize = 32;

/// Requests for a short silent block outside the client's stream, so parameter changes queued for the
/// processor (a program change, edits) reach it: a processor's state only changes when it processes.
#[derive(Default)]
struct Flush {
    requested: AtomicU64,
    done: Mutex<u64>,
    cv: parking_lot::Condvar,
}

impl AudioThread {
    fn start(processor: Processor, channel: Channel, realtime: bool) -> Result<AudioThread> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = channel.request.clone();
        let flush = Arc::new(Flush::default());
        let (s, f) = (stop.clone(), flush.clone());
        // read before the thread starts: the client may send its first block as soon as the load is
        // answered, possibly before the new thread runs
        // SAFETY: the header is mapped for the channel's lifetime; `seq` is atomic.
        let first_seq = unsafe { (*(channel.shm.ptr() as *const ShmHeader)).seq.load(Ordering::Acquire) };
        let handle = std::thread::Builder::new()
            .name("flux-audio".into())
            .spawn(move || {
                if realtime {
                    sys::audio_thread_priority();
                }
                audio_loop(processor, channel, &s, &f, first_seq);
            })
            .context("starting the audio thread")?;
        Ok(AudioThread { stop, wake, flush, handle: Some(handle) })
    }

    /// Have the processor take its queued parameter changes now. False if the audio thread didn't
    /// get to it in time (e.g. a plugin stuck in `process`).
    fn flush(&self, timeout: std::time::Duration) -> bool {
        let generation = self.flush.requested.fetch_add(1, Ordering::AcqRel) + 1;
        self.wake.set();
        let deadline = std::time::Instant::now() + timeout;
        let mut done = self.flush.done.lock();
        while *done < generation {
            if self.flush.cv.wait_until(&mut done, deadline).timed_out() {
                return false;
            }
        }
        true
    }
}

impl Drop for AudioThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake.set();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Stable in-place sort by offset without allocating (event lists are short and almost always sorted).
fn sort_by_offset<T: Copy>(v: &mut [T], key: impl Fn(&T) -> u32) {
    for i in 1..v.len() {
        let x = v[i];
        let mut j = i;
        while j > 0 && key(&v[j - 1]) > key(&x) {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
}

/// Serve blocks until told to stop. No allocation after the first block: the event vectors are
/// preallocated and buffers live in shared memory.
/// `last_seq`: the request sequence number already answered (requests carry the next ones).
fn audio_loop(mut processor: Processor, ch: Channel, stop: &AtomicBool, flush: &Flush, mut last_seq: u64) {
    let base = ch.shm.ptr();
    let h = base as *mut ShmHeader;
    let max_block = processor.setup().max_block;
    let mut midi: Vec<TimedMidi> = Vec::with_capacity(ch.max_events);
    let mut points = Vec::with_capacity(ch.max_params);
    let flush_frames = FLUSH_FRAMES.min(max_block);
    let mut scratch: Vec<Vec<f32>> = (0..ch.out_channels).map(|_| vec![0f32; flush_frames]).collect();
    let mut flushed = 0u64;
    loop {
        match ch.request.wait(100) {
            Wait::Signaled => {}
            Wait::Timeout => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            _ => {
                eprintln!("flux-plugin-host: waiting for audio requests failed; stopping the audio thread");
                break;
            }
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        let want = flush.requested.load(Ordering::Acquire);
        if want != flushed {
            // into scratch buffers: the client may be reading the shared outputs of its last block
            for _ in 0..4 {
                let mut outs: Vec<&mut [f32]> = scratch.iter_mut().map(|b| &mut b[..]).collect();
                processor.process(flush_frames, &[], &mut outs, &[], &[], None);
                if !processor.has_pending_params() {
                    break;
                }
            }
            flushed = want;
            *flush.done.lock() = want;
            flush.cv.notify_all();
        }
        // SAFETY: the header is mapped for the thread's lifetime; `seq` is atomic.
        let seq = unsafe { (*h).seq.load(Ordering::Acquire) };
        if seq == last_seq {
            // woken for a flush or to stop, not for a block
            continue;
        }
        last_seq = seq;
        // SAFETY: `base` maps `ch.layout.size` bytes laid out by ShmLayout with the counts checked at load;
        // the client doesn't touch the request fields or buffers between signalling `request` and seeing
        // `done_seq` reach this block's sequence number.
        unsafe {
            let frames = std::ptr::read_volatile(&raw const (*h).frames) as usize;
            let n_events = (std::ptr::read_volatile(&raw const (*h).n_events) as usize).min(ch.max_events);
            let n_params = (std::ptr::read_volatile(&raw const (*h).n_params) as usize).min(ch.max_params);
            let flags = std::ptr::read_volatile(&raw const (*h).flags);
            let transport = std::ptr::read_volatile(&raw const (*h).transport).to();
            let status = if frames == 0 || frames > max_block {
                -1
            } else {
                midi.clear();
                let ev = base.add(ch.layout.events) as *const ShmMidi;
                for i in 0..n_events {
                    if let Some(m) = std::ptr::read_volatile(ev.add(i)).to()
                        && (m.offset as usize) < frames
                    {
                        midi.push(m);
                    }
                }
                sort_by_offset(&mut midi, |e| e.offset);
                points.clear();
                let pp = base.add(ch.layout.params) as *const ShmParam;
                for i in 0..n_params {
                    let p = std::ptr::read_volatile(pp.add(i)).to();
                    if (p.offset as usize) < frames && p.value.is_finite() {
                        points.push(p);
                    }
                }
                sort_by_offset(&mut points, |p| p.offset);
                let inp = base.add(ch.layout.input) as *const f32;
                let outp = base.add(ch.layout.output) as *mut f32;
                let with_input = flags & 1 != 0;
                let ins: [&[f32]; MAX_CHANNELS as usize] = std::array::from_fn(|c| {
                    if c < ch.in_channels && with_input { std::slice::from_raw_parts(inp.add(c * max_block), frames) } else { &[] }
                });
                let mut outs: [&mut [f32]; MAX_CHANNELS as usize] = std::array::from_fn(|c| {
                    if c < ch.out_channels { std::slice::from_raw_parts_mut(outp.add(c * max_block), frames) } else { &mut [] }
                });
                let n_in = if with_input { ch.in_channels } else { 0 };
                let r = processor.process(frames, &ins[..n_in], &mut outs[..ch.out_channels], &midi, &points, transport.as_ref());
                (*h).skipped = r.skipped as u32;
                (*h).unmapped = r.unmapped;
                (*h).dropped = r.dropped;
                0
            };
            (*h).status = status;
            (*h).latency = processor.latency();
            (*h).done_seq.store(seq, Ordering::Release);
        }
        ch.done.set();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_stably_by_offset() {
        let mut v = [(5u32, 'a'), (1, 'b'), (5, 'c'), (0, 'd'), (1, 'e')];
        sort_by_offset(&mut v, |x| x.0);
        assert_eq!(v, [(0, 'd'), (1, 'b'), (1, 'e'), (5, 'a'), (5, 'c')]);
    }
}
