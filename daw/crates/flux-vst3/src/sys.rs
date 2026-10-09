//! Operating-system pieces of out-of-process hosting: shared memory, named events, a job object
//! that ties plugin hosts to Flux's lifetime, and stdio isolation. Windows only for now.

#![cfg(windows)]

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Threading::*;

fn wide(s: &str) -> Vec<u16> {
    crate::util::wide(s)
}

fn last_error(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{what} failed: {}", std::io::Error::last_os_error())
}

/// A session-local name no other process will pick: `Local\flux-vst3-<pid>-<n>-<kind>`.
pub fn unique_name(kind: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("Local\\flux-vst3-{}-{}-{t:08x}-{kind}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// A named shared-memory section mapped into this process.
pub struct SharedMemory {
    handle: HANDLE,
    ptr: *mut u8,
    size: usize,
}

// SAFETY: the mapping is plain memory; synchronisation is the users' job (see ipc.rs).
unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    pub fn create(name: &str, size: usize) -> Result<SharedMemory> {
        let n = wide(name);
        // SAFETY: plain Win32 calls with valid arguments; results are checked.
        unsafe {
            let h = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, (size as u64 >> 32) as u32, size as u32, n.as_ptr());
            if h.is_null() {
                return Err(last_error("CreateFileMappingW"));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                bail!("shared memory '{name}' already exists");
            }
            Self::map(h, size)
        }
    }

    pub fn open(name: &str, size: usize) -> Result<SharedMemory> {
        let n = wide(name);
        // SAFETY: as above.
        unsafe {
            let h = OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, n.as_ptr());
            if h.is_null() {
                return Err(last_error(&format!("OpenFileMappingW({name})")));
            }
            Self::map(h, size)
        }
    }

    unsafe fn map(h: HANDLE, size: usize) -> Result<SharedMemory> {
        // SAFETY: `h` is a valid mapping handle of at least `size` bytes.
        unsafe {
            let v = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, size);
            if v.Value.is_null() {
                let e = last_error("MapViewOfFile");
                CloseHandle(h);
                return Err(e);
            }
            Ok(SharedMemory { handle: h, ptr: v.Value as *mut u8, size })
        }
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // SAFETY: we own the view and the handle.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr as *mut c_void });
            CloseHandle(self.handle);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Signaled,
    /// The other process's handle was signaled (it exited).
    ProcessExited,
    Timeout,
    Failed,
}

/// A named auto-reset event.
pub struct Event {
    handle: HANDLE,
}

// SAFETY: event handles may be used from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub fn create(name: &str) -> Result<Event> {
        let n = wide(name);
        // SAFETY: plain Win32 call; result checked.
        unsafe {
            let h = CreateEventW(std::ptr::null(), 0, 0, n.as_ptr());
            if h.is_null() {
                return Err(last_error("CreateEventW"));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                bail!("event '{name}' already exists");
            }
            Ok(Event { handle: h })
        }
    }

    pub fn open(name: &str) -> Result<Event> {
        let n = wide(name);
        // SAFETY: plain Win32 call; result checked.
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, 0, n.as_ptr());
            if h.is_null() {
                return Err(last_error(&format!("OpenEventW({name})")));
            }
            Ok(Event { handle: h })
        }
    }

    pub fn set(&self) {
        // SAFETY: valid handle.
        unsafe { SetEvent(self.handle) };
    }

    pub fn wait(&self, timeout_ms: u32) -> Wait {
        // SAFETY: valid handle.
        match unsafe { WaitForSingleObject(self.handle, timeout_ms) } {
            WAIT_OBJECT_0 => Wait::Signaled,
            WAIT_TIMEOUT => Wait::Timeout,
            _ => Wait::Failed,
        }
    }

    /// Wait for the event or for `process` to exit, whichever comes first.
    pub fn wait_or_exit(&self, process: HANDLE, timeout_ms: u32) -> Wait {
        let hs = [self.handle, process];
        // SAFETY: both handles are valid for the duration of the call.
        match unsafe { WaitForMultipleObjects(2, hs.as_ptr(), 0, timeout_ms) } {
            WAIT_OBJECT_0 => Wait::Signaled,
            x if x == WAIT_OBJECT_0 + 1 => Wait::ProcessExited,
            WAIT_TIMEOUT => Wait::Timeout,
            _ => Wait::Failed,
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { CloseHandle(self.handle) };
    }
}

/// A job object that kills every process in it when Flux exits (even if Flux crashes).
pub struct Job {
    handle: HANDLE,
}

// SAFETY: job handles may be used from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub fn new() -> Result<Job> {
        // SAFETY: plain Win32 calls; results checked.
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return Err(last_error("CreateJobObjectW"));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let e = last_error("SetInformationJobObject");
                CloseHandle(h);
                return Err(e);
            }
            Ok(Job { handle: h })
        }
    }

    // handles are opaque values the OS validates, not pointers we dereference
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn assign(&self, process: HANDLE) -> Result<()> {
        // SAFETY: valid handles.
        if unsafe { AssignProcessToJobObject(self.handle, process) } == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: we own the handle (closing it kills the job's processes, as intended at exit).
        unsafe { CloseHandle(self.handle) };
    }
}

/// The job all plugin hosts of this process join.
pub fn job() -> Option<&'static Job> {
    static JOB: std::sync::OnceLock<Option<Job>> = std::sync::OnceLock::new();
    JOB.get_or_init(|| Job::new().ok()).as_ref()
}

unsafe extern "C" {
    fn _dup2(fd1: i32, fd2: i32) -> i32;
}

/// In the plugin host: keep a private handle to the real stdout for the protocol and point everything
/// else that might print (plugins' C runtimes, `println!`) at stderr, so plugin chatter can't corrupt frames.
pub fn take_stdout() -> Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Console::*;
    // SAFETY: plain Win32/CRT calls on this process's own std handles; results checked.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if out.is_null() || out == INVALID_HANDLE_VALUE {
            bail!("the plugin host has no stdout");
        }
        let me = GetCurrentProcess();
        let mut dup: HANDLE = std::ptr::null_mut();
        if DuplicateHandle(me, out, me, &mut dup, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
            return Err(last_error("DuplicateHandle(stdout)"));
        }
        let err = GetStdHandle(STD_ERROR_HANDLE);
        SetStdHandle(STD_OUTPUT_HANDLE, err);
        // the shared C runtime captured the old stdout at startup: point its fd 1 at stderr too
        _dup2(2, 1);
        Ok(std::fs::File::from_raw_handle(dup as _))
    }
}

/// No Windows error dialogs in the plugin host: a crash should end the process so Flux notices.
pub fn quiet_crashes() {
    use windows_sys::Win32::System::Diagnostics::Debug::*;
    // SAFETY: plain Win32 call.
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX) };
}

/// Run the current thread as a pro-audio thread (MMCSS), falling back to time-critical priority.
pub fn audio_thread_priority() {
    // SAFETY: plain Win32 calls on the current thread.
    unsafe {
        let mut idx = 0u32;
        let name = wide("Pro Audio");
        let h = AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut idx);
        if h.is_null() {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
    }
}

/// Whether a process handle has exited (and its exit code).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn exit_code(process: HANDLE) -> Option<u32> {
    let mut code = 0u32;
    // SAFETY: valid process handle.
    unsafe {
        if GetExitCodeProcess(process, &mut code) == 0 || code == STILL_ACTIVE as u32 {
            return None;
        }
    }
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_memory_and_events() {
        let name = unique_name("test-shm");
        let a = SharedMemory::create(&name, 4096).unwrap();
        let b = SharedMemory::open(&name, 4096).unwrap();
        // SAFETY: both views map the same 4096 bytes.
        unsafe {
            *a.ptr().add(100) = 42;
            assert_eq!(*b.ptr().add(100), 42);
        }
        assert!(SharedMemory::create(&name, 4096).is_err());
        let en = unique_name("test-ev");
        let e1 = Event::create(&en).unwrap();
        let e2 = Event::open(&en).unwrap();
        assert_eq!(e2.wait(0), Wait::Timeout);
        e1.set();
        assert_eq!(e2.wait(1000), Wait::Signaled);
        // auto-reset
        assert_eq!(e2.wait(0), Wait::Timeout);
        assert!(Event::open(&unique_name("missing")).is_err());
    }
}
