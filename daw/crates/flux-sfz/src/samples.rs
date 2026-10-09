//! Sample file summaries: how many files, in which formats, how long, how big, and which carry loops.
//!
//! [`summarize`] reads only headers (see [`crate::audio::SampleInfo`]), so summarising a library of thousands
//! of samples takes a moment. [`instrument_samples`] summarises the samples an instrument plays and
//! [`folder_samples`] the WAV/FLAC files of a folder (what `tools/sample_info.py` printed).

use crate::audio::{AudioFormat, SampleInfo};
use crate::error::{Error, Result};
use crate::instrument::{Instrument, SampleRef};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// A summary of sample files.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SampleSummary {
    /// Distinct files.
    pub files: usize,
    /// Files that do not exist.
    pub missing: usize,
    /// Files that exist but could not be read as WAV or FLAC.
    pub unreadable: usize,
    /// Total size on disk of the files read.
    pub total_bytes: u64,
    /// Formats, most common first.
    pub formats: Vec<FormatCount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<Durations>,
    /// Files with at least one loop in their `smpl` chunk (sfizz loops them unless `loop_mode` says otherwise).
    pub with_loops: usize,
    /// Files with a root key in their `smpl` chunk.
    pub with_root_key: usize,
    /// The first few problems (missing or unreadable files).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// How many files share a format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FormatCount {
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub channels: u16,
    /// libsndfile's subtype name: `PCM_16`, `PCM_24`, `FLOAT`...
    pub encoding: String,
    pub files: usize,
}

/// Lengths of the files, in seconds. `median` is the upper median (as `sample_info.py` printed it).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Durations {
    pub min: f64,
    pub median: f64,
    pub max: f64,
    pub total: f64,
}

/// How many problems a summary lists by name.
const MAX_PROBLEMS: usize = 10;

/// Summarise sample files (each path once; the order does not matter).
pub fn summarize<'a>(paths: impl IntoIterator<Item = &'a Path>) -> SampleSummary {
    summarize_with_infos(paths).0
}

/// [`summarize`], also returning the headers that were read, by path.
pub fn summarize_with_infos<'a>(paths: impl IntoIterator<Item = &'a Path>) -> (SampleSummary, HashMap<PathBuf, SampleInfo>) {
    let unique: BTreeSet<&Path> = paths.into_iter().collect();
    let mut infos = HashMap::new();
    let mut s = SampleSummary { files: unique.len(), ..SampleSummary::default() };
    let mut formats: BTreeMap<(AudioFormat, u32, u16, String), usize> = BTreeMap::new();
    let mut durations = vec![];
    for path in unique {
        match SampleInfo::read(path) {
            Ok(info) => {
                s.total_bytes += info.file_size;
                *formats.entry((info.format, info.sample_rate, info.channels, info.encoding())).or_default() += 1;
                durations.push(info.duration());
                s.with_loops += usize::from(!info.loops.is_empty());
                s.with_root_key += usize::from(info.root_key.is_some());
                infos.insert(path.to_path_buf(), info);
            }
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                s.missing += 1;
                if s.problems.len() < MAX_PROBLEMS {
                    s.problems.push(format!("{}: missing", path.display()));
                }
            }
            Err(e) => {
                s.unreadable += 1;
                if s.problems.len() < MAX_PROBLEMS {
                    s.problems.push(e.to_string());
                }
            }
        }
    }
    let mut formats: Vec<FormatCount> = formats
        .into_iter()
        .map(|((format, sample_rate, channels, encoding), files)| FormatCount { format, sample_rate, channels, encoding, files })
        .collect();
    formats.sort_by_key(|f| std::cmp::Reverse(f.files));
    s.formats = formats;
    if !durations.is_empty() {
        durations.sort_by(f64::total_cmp);
        s.duration_s = Some(Durations {
            min: durations[0],
            median: durations[durations.len() / 2],
            max: durations[durations.len() - 1],
            total: durations.iter().sum(),
        });
    }
    (s, infos)
}

/// The sample files an instrument plays (all regions, missing files included).
pub fn instrument_samples(inst: &Instrument) -> SampleSummary {
    instrument_samples_with_infos(inst).0
}

/// [`instrument_samples`], also returning the headers that were read, by path.
pub fn instrument_samples_with_infos(inst: &Instrument) -> (SampleSummary, HashMap<PathBuf, SampleInfo>) {
    let paths: Vec<&Path> = inst
        .regions()
        .iter()
        .filter_map(|r| match r.sample() {
            SampleRef::File(f) => Some(f.path.as_path()),
            _ => None,
        })
        .collect();
    summarize_with_infos(paths)
}

/// The `.wav` and `.flac` files directly inside a folder.
pub fn folder_samples(dir: &Path) -> Result<SampleSummary> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| Error::io(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav") || x.eq_ignore_ascii_case("flac"))
        })
        .collect();
    paths.sort();
    Ok(summarize(paths.iter().map(PathBuf::as_path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, frames: u32, smpl_loop: bool) -> Vec<u8> {
        let data_len = frames * 2;
        let smpl_len: u32 = if smpl_loop { 36 + 24 } else { 0 };
        let mut b = vec![];
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(4 + 24 + 8 + data_len + if smpl_loop { 8 + smpl_len } else { 0 }).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        b.extend(std::iter::repeat_n(0u8, data_len as usize));
        if smpl_loop {
            b.extend_from_slice(b"smpl");
            b.extend_from_slice(&smpl_len.to_le_bytes());
            let mut c = vec![0u32; 9];
            c[3] = 60; // root key
            c[7] = 1; // loops
            for v in c {
                b.extend_from_slice(&v.to_le_bytes());
            }
            for v in [0u32, 0, 10, 100, 0, 0] {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        b
    }

    #[test]
    fn folder_summary() {
        let d = std::env::temp_dir().join(format!("flux-sfz-samples-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.wav"), wav(44100, 44100, true)).unwrap();
        std::fs::write(d.join("b.WAV"), wav(44100, 22050, false)).unwrap();
        std::fs::write(d.join("c.wav"), wav(48000, 96000, false)).unwrap();
        std::fs::write(d.join("bad.flac"), b"nope").unwrap();
        std::fs::write(d.join("notes.txt"), b"x").unwrap();
        let s = folder_samples(&d).unwrap();
        assert_eq!((s.files, s.missing, s.unreadable), (4, 0, 1));
        assert_eq!(s.formats[0], FormatCount { format: AudioFormat::Wav, sample_rate: 44100, channels: 1, encoding: "PCM_16".into(), files: 2 });
        let du = s.duration_s.unwrap();
        assert_eq!((du.min, du.median, du.max, du.total), (0.5, 1.0, 2.0, 3.5));
        assert_eq!((s.with_loops, s.with_root_key), (1, 1));
        let m = summarize([d.join("none.wav").as_path()]);
        assert_eq!((m.files, m.missing), (1, 1));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
