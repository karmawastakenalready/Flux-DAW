//! Small conversions between VST3 C types and Rust: fixed-size strings, class ids, result codes.

use anyhow::{bail, Result};
use std::ffi::c_char;
use vst3::Steinberg::{kResultOk, tresult, TUID};

/// A `char8[N]` buffer (UTF-8, NUL-terminated or full) as a String.
pub fn str8(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A `char16[N]` buffer (UTF-16, NUL-terminated or full) as a String.
pub fn str16(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// A NUL-terminated UTF-16 string pointer as a String (None for null).
///
/// # Safety
/// `p` must be null or point to a NUL-terminated UTF-16 string.
pub unsafe fn str16_ptr(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    // SAFETY: the caller guarantees NUL termination.
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)))
    }
}

/// A NUL-terminated C string pointer as a String (None for null).
///
/// # Safety
/// `p` must be null or point to a NUL-terminated string.
pub unsafe fn str_ptr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees NUL termination.
    Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Write `s` into a `char16[N]` buffer, truncating and always NUL-terminating.
pub fn write16(s: &str, dst: &mut [u16]) {
    if dst.is_empty() {
        return;
    }
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16()) {
        *d = c;
        n += 1;
    }
    let end = n.min(dst.len() - 1);
    dst[end] = 0;
}

/// UTF-16 with a terminating NUL.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Byte order of a TUID's canonical string relative to its memory: on Windows a TUID is laid out as a COM
/// GUID (first three fields little-endian), elsewhere as plain big-endian bytes. The canonical string
/// (moduleinfo.json, .vstpreset headers, `FUID::toString`) is the same on every platform.
#[cfg(windows)]
const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
#[cfg(not(windows))]
const ORDER: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// A class id as its canonical 32-digit upper-case hex string (as in moduleinfo.json and presets).
pub fn cid_hex(cid: &TUID) -> String {
    ORDER.iter().map(|&i| format!("{:02X}", cid[i] as u8)).collect()
}

/// Parse a canonical 32-digit hex class id.
pub fn parse_cid(s: &str) -> Result<TUID> {
    let s = s.trim();
    if s.len() != 32 || !s.is_ascii() {
        bail!("class id '{s}' must be 32 hex digits");
    }
    let mut out: TUID = [0; 16];
    for (k, &i) in ORDER.iter().enumerate() {
        let b = u8::from_str_radix(&s[2 * k..2 * k + 2], 16).map_err(|_| anyhow::anyhow!("class id '{s}' is not hex"))?;
        out[i] = b as c_char;
    }
    Ok(out)
}

/// Name of a VST3 result code for error messages.
pub fn result_name(r: tresult) -> String {
    use vst3::Steinberg::*;
    match r {
        x if x == kResultOk => "kResultOk".into(),
        x if x == kResultFalse => "kResultFalse".into(),
        x if x == kNoInterface => "kNoInterface".into(),
        x if x == kInvalidArgument => "kInvalidArgument".into(),
        x if x == kNotImplemented => "kNotImplemented".into(),
        x if x == kInternalError => "kInternalError".into(),
        x if x == kNotInitialized => "kNotInitialized".into(),
        x if x == kOutOfMemory => "kOutOfMemory".into(),
        x => format!("result {x:#x}"),
    }
}

/// `Ok(())` for kResultOk, else an error naming the call.
pub fn check(r: tresult, what: &str) -> Result<()> {
    if r == kResultOk {
        Ok(())
    } else {
        bail!("{what} failed ({})", result_name(r))
    }
}

/// Snake-case form of a parameter title, as pedalboard exposes plugin parameters ("A Osc 1 Type" -> "a_osc_1_type").
pub fn snake(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut gap = false;
    for c in title.chars() {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('_');
            }
            gap = false;
            out.extend(c.to_lowercase());
        } else {
            gap = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_round_trip() {
        let mut buf = [0u16; 8];
        write16("Surge XT synth", &mut buf);
        assert_eq!(str16(&buf), "Surge X");
        write16("ab", &mut buf);
        assert_eq!(str16(&buf), "ab");
        let w = wide("hé");
        assert_eq!(unsafe { str16_ptr(w.as_ptr()) }.as_deref(), Some("hé"));
    }

    #[test]
    fn class_ids() {
        let cid = parse_cid("ABCDEF0123456789ABCDEF0123456789").unwrap();
        assert_eq!(cid_hex(&cid), "ABCDEF0123456789ABCDEF0123456789");
        // the SDK's uid() macro builds the memory layout; its canonical string is the four numbers in order
        let u = vst3::uid(0xABCDEF01, 0x9182FAEB, 0x44475342, 0x44657864);
        assert_eq!(cid_hex(&u), "ABCDEF019182FAEB4447534244657864");
        assert!(parse_cid("xyz").is_err());
        assert!(parse_cid("ZZCDEF0123456789ABCDEF0123456789").is_err());
    }

    #[test]
    fn snake_case_titles() {
        assert_eq!(snake("A Osc 1 Type"), "a_osc_1_type");
        assert_eq!(snake("  Filter-1 Cutoff (Hz) "), "filter_1_cutoff_hz");
        assert_eq!(snake("Volume"), "volume");
    }
}
