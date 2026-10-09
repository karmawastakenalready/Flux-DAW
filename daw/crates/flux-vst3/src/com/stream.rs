//! `IBStream` over a byte vector: what plugins write their state into and read it back from.

use parking_lot::Mutex;
use std::ffi::c_void;
use vst3::Steinberg::IBStream_::IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet};
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper};

struct Inner {
    data: Vec<u8>,
    pos: usize,
}

/// A growable in-memory stream. Plugins call it from the thread the host called them on.
pub struct MemStream {
    inner: Mutex<Inner>,
}

impl Class for MemStream {
    type Interfaces = (IBStream, ISizeableStream);
}

impl MemStream {
    pub fn new(data: Vec<u8>) -> ComWrapper<MemStream> {
        ComWrapper::new(MemStream { inner: Mutex::new(Inner { data, pos: 0 }) })
    }

    pub fn empty() -> ComWrapper<MemStream> {
        Self::new(Vec::new())
    }

    /// Everything written so far.
    pub fn bytes(&self) -> Vec<u8> {
        self.inner.lock().data.clone()
    }

    /// Bytes the reader hasn't consumed yet (how much of a state a plugin left unread).
    pub fn remaining(&self) -> usize {
        let g = self.inner.lock();
        g.data.len().saturating_sub(g.pos)
    }

    pub fn rewind(&self) {
        self.inner.lock().pos = 0;
    }
}

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        if num_bytes < 0 || (buffer.is_null() && num_bytes > 0) {
            return kInvalidArgument;
        }
        let mut g = self.inner.lock();
        let n = (num_bytes as usize).min(g.data.len().saturating_sub(g.pos));
        if n > 0 {
            // SAFETY: the plugin guarantees `buffer` holds `num_bytes` bytes; n <= num_bytes.
            unsafe { std::ptr::copy_nonoverlapping(g.data.as_ptr().add(g.pos), buffer as *mut u8, n) };
        }
        g.pos += n;
        if !num_read.is_null() {
            // SAFETY: optional out-pointer supplied by the plugin.
            unsafe { *num_read = n as int32 };
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_written: *mut int32) -> tresult {
        if num_bytes < 0 || (buffer.is_null() && num_bytes > 0) {
            return kInvalidArgument;
        }
        let n = num_bytes as usize;
        let mut g = self.inner.lock();
        let end = g.pos + n;
        if g.data.len() < end {
            g.data.resize(end, 0);
        }
        if n > 0 {
            let pos = g.pos;
            // SAFETY: the plugin guarantees `buffer` holds `num_bytes` bytes.
            unsafe { std::ptr::copy_nonoverlapping(buffer as *const u8, g.data.as_mut_ptr().add(pos), n) };
        }
        g.pos = end;
        if !num_written.is_null() {
            // SAFETY: optional out-pointer supplied by the plugin.
            unsafe { *num_written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let mut g = self.inner.lock();
        let base = match mode {
            m if m == kIBSeekSet as int32 => 0,
            m if m == kIBSeekCur as int32 => g.pos as i64,
            m if m == kIBSeekEnd as int32 => g.data.len() as i64,
            _ => return kInvalidArgument,
        };
        let Some(new) = base.checked_add(pos).filter(|&p| p >= 0) else { return kInvalidArgument };
        // seeking past the end is allowed (a later write fills the gap with zeros)
        g.pos = new as usize;
        if !result.is_null() {
            // SAFETY: optional out-pointer supplied by the plugin.
            unsafe { *result = new };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        unsafe { *pos = self.inner.lock().pos as int64 };
        kResultOk
    }
}

impl ISizeableStreamTrait for MemStream {
    unsafe fn getStreamSize(&self, size: *mut int64) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        unsafe { *size = self.inner.lock().data.len() as int64 };
        kResultOk
    }

    unsafe fn setStreamSize(&self, size: int64) -> tresult {
        if size < 0 {
            return kInvalidArgument;
        }
        self.inner.lock().data.resize(size as usize, 0);
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComPtr;

    #[test]
    fn write_seek_read() {
        let s = MemStream::empty();
        let p: ComPtr<IBStream> = s.to_com_ptr().unwrap();
        unsafe {
            let mut data = *b"hello world";
            let mut n = 0;
            assert_eq!(p.write(data.as_mut_ptr() as *mut c_void, 11, &mut n), kResultOk);
            assert_eq!(n, 11);
            let mut at = 0;
            assert_eq!(p.seek(6, kIBSeekSet as int32, &mut at), kResultOk);
            assert_eq!(at, 6);
            let mut buf = [0u8; 16];
            assert_eq!(p.read(buf.as_mut_ptr() as *mut c_void, 16, &mut n), kResultOk);
            assert_eq!(&buf[..n as usize], b"world");
            assert_eq!(p.seek(-1, kIBSeekSet as int32, std::ptr::null_mut()), kInvalidArgument);
            // past-the-end seek then write zero-fills
            p.seek(2, kIBSeekEnd as int32, std::ptr::null_mut());
            let mut x = *b"!";
            p.write(x.as_mut_ptr() as *mut c_void, 1, std::ptr::null_mut());
        }
        assert_eq!(s.bytes(), b"hello world\0\0!");
    }
}
