//! `flux.com`: Flux's commands in a terminal. The installed `flux.exe` is a windowed program (opening it from the
//! Start menu shows no console), and a terminal doesn't wait for those; Windows runs `flux.com` before `flux.exe`
//! when `flux` is typed, and this starts the `flux.exe` beside it with the same arguments and this terminal. A
//! command is waited for (its exit code is this program's) and ends with it (Ctrl+C, a closed terminal); opening
//! the app's window returns at once.
//!
//! Built by `build.rs` with rustc alone (no crates), so the few Windows calls are declared here.

mod window;

use std::ffi::{OsString, c_void};
use std::os::windows::io::AsRawHandle;
use std::process::{Command, ExitCode, Stdio};

#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    per_process_user_time: i64,
    per_job_user_time: i64,
    limit_flags: u32,
    min_working_set: usize,
    max_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

/// JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
}

/// A job that ends the processes in it when this program exits (its handle, never closed, closes then).
fn kill_on_exit_job() -> Option<*mut c_void> {
    // SAFETY: kernel32 calls with null optional arguments and a limits structure laid out as Windows defines it,
    // which outlives the call.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut limits = ExtendedLimits::default();
        limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&raw const limits).cast(), size_of::<ExtendedLimits>() as u32);
        (ok != 0).then_some(job)
    }
}

fn main() -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(p) => p.with_file_name("flux.exe"),
        Err(e) => {
            eprintln!("flux: {e}");
            return ExitCode::from(2);
        }
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut cmd = Command::new(&exe);
    cmd.args(&args);
    if window::opens_window(&args) {
        // the window doesn't keep this terminal
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        return match cmd.spawn() {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("flux: couldn't start {}: {e}", exe.display());
                ExitCode::from(2)
            }
        };
    }
    let job = kill_on_exit_job();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flux: couldn't start {}: {e}", exe.display());
            return ExitCode::from(2);
        }
    };
    if let Some(job) = job {
        // SAFETY: both handles are open. Failing only loses the clean-up of a command left running.
        unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) };
    }
    match child.wait() {
        // the whole code: a crash's NTSTATUS too
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("flux: {e}");
            ExitCode::from(2)
        }
    }
}
