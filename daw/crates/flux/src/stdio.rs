//! MCP speaks JSON-RPC over stdout, but native libraries (sfizz) print status lines to stdout too.
//! `take_stdout` points the process's stdout at stderr (C runtime fd 1 and the OS std handle, so CRTs
//! initialised later inherit it) and returns a private handle to the real stdout for protocol messages.
//! Call it before anything loads sfizz.

use std::fs::File;

#[cfg(windows)]
pub fn take_stdout() -> File {
    use std::os::windows::io::FromRawHandle;
    unsafe extern "system" {
        fn GetStdHandle(n: u32) -> *mut core::ffi::c_void;
        fn SetStdHandle(n: u32, h: *mut core::ffi::c_void) -> i32;
    }
    const STD_OUTPUT: u32 = -11i32 as u32;
    const STD_ERROR: u32 = -12i32 as u32;
    unsafe {
        let saved = libc::dup(1);
        let handle = if saved >= 0 { libc::get_osfhandle(saved) as *mut core::ffi::c_void } else { GetStdHandle(STD_OUTPUT) };
        libc::dup2(2, 1);
        SetStdHandle(STD_OUTPUT, GetStdHandle(STD_ERROR));
        // owned by the CRT descriptor `saved`, which is never closed: the File must never be dropped
        File::from_raw_handle(handle)
    }
}

#[cfg(unix)]
pub fn take_stdout() -> File {
    use std::os::unix::io::FromRawFd;
    unsafe {
        let saved = libc::dup(1);
        libc::dup2(2, 1);
        File::from_raw_fd(saved)
    }
}
