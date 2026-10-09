//! `.vstpreset` files (the VST3 SDK's preset format), which Flux also uses to store plugin state.
//!
//! Layout (little-endian):
//! ```text
//! header   "VST3"  i32 version (1)  32 ASCII hex class id  i64 offset of the chunk list
//! data     chunk bytes ...
//! list     "List"  i32 count  { 4-byte id, i64 offset, i64 size } * count
//! ```
//! Chunks: `Comp` (component state), `Cont` (controller state), `Info` (XML metadata).

use crate::plugin::State;
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

const HEADER: usize = 4 + 4 + 32 + 8;
const VERSION: i32 = 1;

/// A preset: the class it belongs to, its state and optional XML metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preset {
    /// Canonical 32-digit class id of the processor.
    pub class_id: String,
    pub state: State,
    /// The `Info` chunk (MetaInfo XML), if present.
    pub info: Option<Vec<u8>>,
}

impl Preset {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        if self.class_id.len() != 32 || !self.class_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("class id '{}' must be 32 hex digits", self.class_id);
        }
        let mut chunks: Vec<(&[u8; 4], &[u8])> = vec![(b"Comp", &self.state.component)];
        if !self.state.controller.is_empty() {
            chunks.push((b"Cont", &self.state.controller));
        }
        if let Some(i) = &self.info {
            chunks.push((b"Info", i));
        }
        let data_len: usize = chunks.iter().map(|c| c.1.len()).sum();
        let mut out = Vec::with_capacity(HEADER + data_len + 8 + chunks.len() * 20);
        out.extend_from_slice(b"VST3");
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(self.class_id.to_ascii_uppercase().as_bytes());
        out.extend_from_slice(&((HEADER + data_len) as i64).to_le_bytes());
        let mut entries = vec![];
        for (id, data) in &chunks {
            entries.push((**id, out.len() as i64, data.len() as i64));
            out.extend_from_slice(data);
        }
        out.extend_from_slice(b"List");
        out.extend_from_slice(&(entries.len() as i32).to_le_bytes());
        for (id, off, size) in entries {
            out.extend_from_slice(&id);
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
        }
        Ok(out)
    }

    pub fn from_bytes(b: &[u8]) -> Result<Preset> {
        if b.len() < HEADER || &b[..4] != b"VST3" {
            bail!("not a .vstpreset (no VST3 header)");
        }
        let version = i32::from_le_bytes(b[4..8].try_into()?);
        if version < 1 {
            bail!(".vstpreset version {version} is not supported");
        }
        let class_id = std::str::from_utf8(&b[8..40]).map_err(|_| anyhow!(".vstpreset class id is not ASCII"))?.to_ascii_uppercase();
        if !class_id.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!(".vstpreset class id '{class_id}' is not hex");
        }
        let list = usize::try_from(i64::from_le_bytes(b[40..48].try_into()?)).map_err(|_| anyhow!("bad chunk list offset"))?;
        if list.checked_add(8).is_none_or(|e| e > b.len()) || &b[list..list + 4] != b"List" {
            bail!(".vstpreset chunk list is missing or out of range");
        }
        let count = i32::from_le_bytes(b[list + 4..list + 8].try_into()?);
        if !(0..=1024).contains(&count) {
            bail!(".vstpreset has {count} chunks");
        }
        let mut p = Preset { class_id, ..Default::default() };
        for k in 0..count as usize {
            let e = list + 8 + k * 20;
            let entry = b.get(e..e + 20).ok_or_else(|| anyhow!(".vstpreset chunk list is truncated"))?;
            let id = &entry[..4];
            let off = usize::try_from(i64::from_le_bytes(entry[4..12].try_into()?)).map_err(|_| anyhow!("bad chunk offset"))?;
            let size = usize::try_from(i64::from_le_bytes(entry[12..20].try_into()?)).map_err(|_| anyhow!("bad chunk size"))?;
            let data = off
                .checked_add(size)
                .and_then(|end| b.get(off..end))
                .ok_or_else(|| anyhow!(".vstpreset chunk {} is out of range", String::from_utf8_lossy(id)))?;
            match id {
                b"Comp" => p.state.component = data.to_vec(),
                b"Cont" => p.state.controller = data.to_vec(),
                b"Info" => p.info = Some(data.to_vec()),
                _ => {}
            }
        }
        Ok(p)
    }

    pub fn load(path: &Path) -> Result<Preset> {
        let b = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Preset::from_bytes(&b).with_context(|| format!("reading {}", path.display()))
    }

    /// Write atomically (temp file + rename) so a crash never leaves half a preset.
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = self.to_bytes()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("vstpreset.tmp");
        std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = Preset {
            class_id: "ABCDEF019182FAEB566D624153675854".into(),
            state: State { component: vec![1, 2, 3, 4, 5], controller: vec![9, 8] },
            info: Some(b"<MetaInfo/>".to_vec()),
        };
        let b = p.to_bytes().unwrap();
        assert_eq!(&b[..4], b"VST3");
        assert_eq!(Preset::from_bytes(&b).unwrap(), p);
        let q = Preset { info: None, state: State { component: vec![7], controller: vec![] }, ..p };
        assert_eq!(Preset::from_bytes(&q.to_bytes().unwrap()).unwrap(), q);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Preset::from_bytes(b"nope").is_err());
        let mut b = Preset { class_id: "ABCDEF019182FAEB566D624153675854".into(), ..Default::default() }.to_bytes().unwrap();
        let n = b.len();
        b[n - 1] = 0x7f; // chunk size now huge
        assert!(Preset::from_bytes(&b).is_err());
        let mut c = b.clone();
        c[40..48].copy_from_slice(&(1i64 << 40).to_le_bytes());
        assert!(Preset::from_bytes(&c).is_err());
        assert!(Preset { class_id: "xyz".into(), ..Default::default() }.to_bytes().is_err());
    }
}
