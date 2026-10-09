//! `IMessage` and `IAttributeList`: the objects a plugin asks the host to create (through
//! `IHostApplication::createInstance`) to send messages between its processor and its controller.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper};

enum Value {
    Int(i64),
    Float(f64),
    /// UTF-16 including the terminating NUL.
    Str(Vec<u16>),
    Bin(Vec<u8>),
}

/// Typed key/value storage.
pub struct AttributeList {
    map: Mutex<HashMap<CString, Value>>,
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl AttributeList {
    pub fn new() -> ComWrapper<AttributeList> {
        ComWrapper::new(AttributeList { map: Mutex::new(HashMap::new()) })
    }
}

/// # Safety
/// `id` must be null or a NUL-terminated string.
unsafe fn key(id: AttrID) -> Option<CString> {
    if id.is_null() {
        return None;
    }
    // SAFETY: per the caller.
    Some(unsafe { CStr::from_ptr(id) }.to_owned())
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        self.map.lock().insert(k, Value::Int(value));
        kResultOk
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        match self.map.lock().get(&k) {
            Some(Value::Int(v)) if !value.is_null() => {
                // SAFETY: checked non-null.
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        self.map.lock().insert(k, Value::Float(value));
        kResultOk
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        match self.map.lock().get(&k) {
            Some(Value::Float(v)) if !value.is_null() => {
                // SAFETY: checked non-null.
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut v = Vec::new();
        let mut i = 0;
        loop {
            // SAFETY: the plugin passes a NUL-terminated UTF-16 string.
            let c = unsafe { *string.add(i) };
            v.push(c as u16);
            if c == 0 {
                break;
            }
            i += 1;
        }
        self.map.lock().insert(k, Value::Str(v));
        kResultOk
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        let cap = size_in_bytes as usize / 2;
        if string.is_null() || cap == 0 {
            return kInvalidArgument;
        }
        match self.map.lock().get(&k) {
            Some(Value::Str(v)) => {
                let n = v.len().min(cap);
                // SAFETY: the plugin's buffer holds `cap` UTF-16 units; we write at most that and terminate.
                unsafe {
                    std::ptr::copy_nonoverlapping(v.as_ptr() as *const TChar, string, n);
                    *string.add(n - 1) = 0;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        if data.is_null() && size_in_bytes > 0 {
            return kInvalidArgument;
        }
        let v = if size_in_bytes == 0 {
            Vec::new()
        } else {
            // SAFETY: the plugin guarantees `data` holds `size_in_bytes` bytes.
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }.to_vec()
        };
        self.map.lock().insert(k, Value::Bin(v));
        kResultOk
    }

    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        let Some(k) = (unsafe { key(id) }) else { return kInvalidArgument };
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        match self.map.lock().get(&k) {
            Some(Value::Bin(v)) => {
                // The pointer stays valid until the attribute is replaced or the list is released, as in the SDK.
                // SAFETY: both out-pointers checked non-null.
                unsafe {
                    *data = v.as_ptr() as *const c_void;
                    *size_in_bytes = v.len() as uint32;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

/// A message with an id and an attribute list.
pub struct Message {
    id: Mutex<Option<CString>>,
    attrs: ComWrapper<AttributeList>,
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl Message {
    pub fn new() -> ComWrapper<Message> {
        ComWrapper::new(Message { id: Mutex::new(None), attrs: AttributeList::new() })
    }
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        // The CString's heap buffer doesn't move while the message lives (setMessageID replaces it).
        self.id.lock().as_ref().map_or(std::ptr::null(), |s| s.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        // SAFETY: the plugin passes null or a NUL-terminated string.
        *self.id.lock() = unsafe { key(id) };
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Not add-ref'd: the SDK's HostMessage returns a borrowed pointer too.
        self.attrs.as_com_ref::<IAttributeList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComPtr;

    #[test]
    fn attributes_round_trip() {
        let l = AttributeList::new();
        let p: ComPtr<IAttributeList> = l.to_com_ptr().unwrap();
        unsafe {
            assert_eq!(p.setInt(c"n".as_ptr(), 42), kResultOk);
            let mut n = 0;
            assert_eq!(p.getInt(c"n".as_ptr(), &mut n), kResultOk);
            assert_eq!(n, 42);
            assert_eq!(p.getFloat(c"n".as_ptr(), &mut 0.0), kResultFalse);
            let s = crate::util::wide("héllo");
            p.setString(c"s".as_ptr(), s.as_ptr() as *const TChar);
            let mut buf = [0 as TChar; 4];
            assert_eq!(p.getString(c"s".as_ptr(), buf.as_mut_ptr(), 8), kResultOk);
            assert_eq!(crate::util::str16(&buf.map(|c| c as u16)), "hél");
            let blob = [1u8, 2, 3];
            p.setBinary(c"b".as_ptr(), blob.as_ptr() as *const c_void, 3);
            let (mut d, mut len) = (std::ptr::null(), 0);
            assert_eq!(p.getBinary(c"b".as_ptr(), &mut d, &mut len), kResultOk);
            assert_eq!(std::slice::from_raw_parts(d as *const u8, len as usize), &blob);
        }
    }

    #[test]
    fn message_id() {
        let m = Message::new();
        let p: ComPtr<IMessage> = m.to_com_ptr().unwrap();
        unsafe {
            assert!(p.getMessageID().is_null());
            p.setMessageID(c"hello".as_ptr());
            assert_eq!(CStr::from_ptr(p.getMessageID()).to_str().unwrap(), "hello");
            assert!(!p.getAttributes().is_null());
        }
    }
}
