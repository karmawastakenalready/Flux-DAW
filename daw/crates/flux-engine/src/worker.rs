//! Offline SFZ and rig renders in worker processes.
//!
//! sfizz keeps process-wide state: one random generator drives random round robins (`lorand`/`hirand`),
//! `amp_random`, `pitch_random` and random LFOs for every synth in the process. Rendered in-process, a
//! track would sound different depending on what the process rendered before (and, with tracks rendered in
//! parallel, on timing). So each offline SFZ or rig render runs in a fresh worker process (`flux
//! render-worker`): the same project always renders to the same audio, and an instrument that crashes
//! takes only its worker down. Live playback stays in-process.
//!
//! Protocol: the job (the project with only the track to render, and the length) as JSON on the worker's
//! stdin; back on its stdout a JSON header line, then the left and the right channel as little-endian f32.
//! The worker's stderr (and anything native libraries print) is kept for error messages.

use crate::dsp::Stereo;
use crate::paths;
use crate::render::render_instrument_here;
use anyhow::{Context, Result, anyhow, bail};
use flux_core::{Project, TimeMap, Track};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Lines of a worker's stderr kept for error messages.
const LOG_LINES: usize = 20;

/// How workers are started: an executable that runs [`main`] when given `render-worker` after `args`.
#[derive(Clone, Debug)]
pub struct WorkerCommand {
    pub exe: PathBuf,
    pub args: Vec<String>,
}

static COMMAND: OnceLock<WorkerCommand> = OnceLock::new();

/// Set once at startup (Flux runs its own executable). Without it, renders run in-process, which is
/// deterministic only for the first sfizz render of the process: fine for tools and tests that render once.
pub fn set_command(c: WorkerCommand) {
    let _ = COMMAND.set(c);
}

/// Whether renders go to worker processes.
pub fn enabled() -> bool {
    COMMAND.get().is_some()
}

#[derive(Serialize, Deserialize)]
struct Job {
    /// The project with only the track to render.
    project: Project,
    track: u64,
    frames: usize,
}

#[derive(Serialize, Deserialize)]
struct Header {
    /// Frames per channel that follow (0 on error).
    frames: usize,
    note: String,
    error: Option<String>,
}

/// Render a track's instrument in a fresh worker process (see the module documentation), or in this
/// process when no worker command is set.
pub fn render(track: &Track, p: &Project, tm: &TimeMap, frames: usize) -> Result<(Stereo, String)> {
    let Some(cmd) = COMMAND.get() else { return render_instrument_here(track, p, tm, frames) };
    let mut project = p.clone();
    project.tracks = vec![track.clone()];
    let job = serde_json::to_vec(&Job { project, track: track.id, frames })?;

    let mut child = Command::new(&cmd.exe)
        .args(&cmd.args)
        .arg("render-worker")
        .env("FLUX_ROOT", paths::root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("starting a render worker ({})", cmd.exe.display()))?;
    if let Some(j) = flux_vst3::sys::job() {
        // without it the worker just isn't killed if Flux crashes
        let _ = j.assign(child.as_raw_handle() as _);
    }
    let log = Arc::new(Mutex::new(VecDeque::new()));
    let stderr = child.stderr.take().context("worker stderr")?;
    let l2 = log.clone();
    let reader = std::thread::Builder::new().name("render-worker-log".into()).spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut l = l2.lock().unwrap_or_else(|e| e.into_inner());
            if l.len() == LOG_LINES {
                l.pop_front();
            }
            l.push_back(line);
        }
    })?;
    let mut w = Worker { child, log, reader: Some(reader) };

    // the worker reads its whole job before writing anything, so this can't deadlock
    let sent = w.child.stdin.take().context("worker stdin").and_then(|mut i| Ok(i.write_all(&job)?));
    if sent.is_err() {
        return Err(w.failed("stopped before taking the job"));
    }
    let mut out = BufReader::with_capacity(1 << 16, w.child.stdout.take().context("worker stdout")?);
    let mut line = String::new();
    let header: Header = match out.read_line(&mut line).ok().and_then(|_| serde_json::from_str(&line).ok()) {
        Some(h) => h,
        None => return Err(w.failed("stopped")),
    };
    if let Some(e) = header.error {
        let _ = w.child.wait();
        bail!("{e}");
    }
    let (Ok(mut left), Ok(mut right)) = (read_f32(&mut out, header.frames), read_f32(&mut out, header.frames)) else {
        return Err(w.failed("stopped while sending audio"));
    };
    if !w.child.wait()?.success() {
        return Err(w.failed("failed after sending audio"));
    }
    left.resize(frames, 0.0);
    right.resize(frames, 0.0);
    Ok(((left, right), header.note))
}

/// A running worker, for error messages.
struct Worker {
    child: Child,
    log: Arc<Mutex<VecDeque<String>>>,
    reader: Option<JoinHandle<()>>,
}

impl Worker {
    /// End the worker and say why it failed, with the end of its log.
    fn failed(&mut self, what: &str) -> anyhow::Error {
        // it may still be writing to a pipe nobody reads
        let _ = self.child.kill();
        let status = self.child.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        let tail: Vec<String> = self.log.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take(3).rev().cloned().collect();
        let tail = if tail.is_empty() { String::new() } else { format!(": {}", tail.join(" | ")) };
        anyhow!("the render worker {what} ({status}){tail}")
    }
}

fn read_f32(r: &mut impl Read, n: usize) -> std::io::Result<Vec<f32>> {
    let mut out = Vec::with_capacity(n);
    let mut buf = vec![0u8; 1 << 16];
    while out.len() < n {
        let take = ((n - out.len()) * 4).min(buf.len());
        r.read_exact(&mut buf[..take])?;
        out.extend(buf[..take].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)));
    }
    Ok(out)
}

fn write_f32(w: &mut impl Write, x: &[f32]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1 << 16);
    for chunk in x.chunks(1 << 14) {
        buf.clear();
        buf.extend(chunk.iter().flat_map(|v| v.to_le_bytes()));
        w.write_all(&buf)?;
    }
    Ok(())
}

/// The worker process: one job from stdin, the audio to stdout. Exit code 0 when the header was sent
/// (including a render error, reported in it), 2 when the job couldn't be read or answered.
pub fn main() -> ExitCode {
    flux_vst3::sys::quiet_crashes();
    // sfizz prints to stdout: keep the real one for the answer, before anything loads it
    let out = match flux_vst3::sys::take_stdout() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("render-worker: {e:#}");
            return ExitCode::from(2);
        }
    };
    let mut out = BufWriter::with_capacity(1 << 16, out);
    let mut text = String::new();
    let job: Job = match std::io::stdin().read_to_string(&mut text).map_err(anyhow::Error::from).and_then(|_| Ok(serde_json::from_str(&text)?)) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("render-worker: reading the job: {e:#}");
            return ExitCode::from(2);
        }
    };
    let result = job
        .project
        .tracks
        .iter()
        .find(|t| t.id == job.track)
        .context("the job has no such track")
        .and_then(|t| render_instrument_here(t, &job.project, &TimeMap::new(&job.project), job.frames));
    let sent = match result {
        Ok(((l, r), note)) => {
            let frames = l.len().min(r.len());
            let h = Header { frames, note, error: None };
            serde_json::to_writer(&mut out, &h)
                .map_err(std::io::Error::other)
                .and_then(|_| out.write_all(b"\n"))
                .and_then(|_| write_f32(&mut out, &l[..frames]))
                .and_then(|_| write_f32(&mut out, &r[..frames]))
        }
        Err(e) => {
            let h = Header { frames: 0, note: String::new(), error: Some(format!("{e:#}")) };
            serde_json::to_writer(&mut out, &h).map_err(std::io::Error::other).and_then(|_| out.write_all(b"\n"))
        }
    };
    match sent.and_then(|_| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("render-worker: sending the result: {e}");
            ExitCode::from(2)
        }
    }
}
