//! Song structure for the analyses: named time ranges ([`Section`]), bar-to-time maps ([`TempoMap`]) and the
//! two metadata files that carry them, Flux's layered-export manifest ([`LayersManifest`], `<stem>.layers.json`)
//! and the Python pipeline's song metadata ([`PipelineMeta`], `output/<title>/<title>.json`).

use crate::audio::AudioRef;
use anyhow::{Context, Result, bail, ensure};
use flux_engine::RenderOutput;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A named time range of a song, in seconds from its start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub name: String,
    pub start_s: f64,
    pub end_s: f64,
}

impl Section {
    pub fn new(name: impl Into<String>, start_s: f64, end_s: f64) -> Section {
        Section { name: name.into(), start_s, end_s }
    }

    pub fn duration_s(&self) -> f64 {
        self.end_s - self.start_s
    }

    /// One section covering all of `a`, named "all".
    pub fn whole(a: AudioRef) -> Section {
        Section::new("all", 0.0, a.duration_s())
    }

    /// The sections of a render (`RenderOutput::sections`), in seconds.
    pub fn of_render(out: &RenderOutput) -> Vec<Section> {
        let sr = out.sr as f64;
        out.sections.iter().map(|s| Section::new(s.name.clone(), s.start as f64 / sr, s.end as f64 / sr)).collect()
    }

    /// Sample range `[a, b)` of this section at `sr`, clamped to `len` (Python's `int(t * sr)` truncation).
    pub fn samples(&self, sr: u32, len: usize) -> (usize, usize) {
        let at = |t: f64| ((t.max(0.0) * sr as f64) as usize).min(len);
        let a = at(self.start_s);
        (a, at(self.end_s).max(a))
    }
}

/// Check that sections are usable on a signal of `duration_s`: finite, non-empty, starting inside the signal.
pub fn check_sections(sections: &[Section], duration_s: f64) -> Result<()> {
    for s in sections {
        ensure!(s.start_s.is_finite() && s.end_s.is_finite(), "section '{}' has a non-finite time", s.name);
        ensure!(s.end_s > s.start_s, "section '{}' ends ({:.3} s) before it starts ({:.3} s)", s.name, s.end_s, s.start_s);
        ensure!(
            s.start_s >= 0.0 && s.start_s < duration_s,
            "section '{}' starts at {:.3} s, outside the {:.3} s signal",
            s.name,
            s.start_s,
            duration_s
        );
    }
    Ok(())
}

/// One tempo segment: from `bar` on, `bpm` quarter notes per minute, starting at `start_s`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoSeg {
    pub bar: u32,
    pub bpm: f64,
    pub start_s: f64,
}

/// Bar numbers (1-based) to seconds, for a song whose tempo changes at bar lines.
#[derive(Clone, Debug, PartialEq)]
pub struct TempoMap {
    segs: Vec<TempoSeg>,
    beats_per_bar: f64,
}

impl TempoMap {
    /// A tempo map from its segments (sorted by bar; the first must be bar 1).
    pub fn new(mut segs: Vec<TempoSeg>, beats_per_bar: f64) -> Result<TempoMap> {
        ensure!(!segs.is_empty(), "empty tempo map");
        ensure!(beats_per_bar > 0.0, "beats per bar must be positive");
        segs.sort_by_key(|s| s.bar);
        ensure!(segs[0].bar <= 1, "the tempo map starts at bar {}, not bar 1", segs[0].bar);
        for s in &segs {
            ensure!(s.bpm > 0.0 && s.bpm.is_finite(), "tempo {} BPM at bar {} is not a positive number", s.bpm, s.bar);
        }
        Ok(TempoMap { segs, beats_per_bar })
    }

    /// One tempo for the whole song.
    pub fn constant(bpm: f64, beats_per_bar: f64) -> Result<TempoMap> {
        TempoMap::new(vec![TempoSeg { bar: 1, bpm, start_s: 0.0 }], beats_per_bar)
    }

    /// Start of `bar` in seconds (the pipeline's `bar_time`: the segment's start plus whole bars at its tempo).
    pub fn bar_time(&self, bar: u32) -> f64 {
        let seg = self.segs.iter().rev().find(|s| s.bar <= bar).unwrap_or(&self.segs[0]);
        seg.start_s + (bar as f64 - seg.bar as f64) * self.beats_per_bar * 60.0 / seg.bpm
    }

    /// The tempo in effect at `bar`.
    pub fn bpm_at(&self, bar: u32) -> f64 {
        self.segs.iter().rev().find(|s| s.bar <= bar).unwrap_or(&self.segs[0]).bpm
    }

    pub fn segments(&self) -> &[TempoSeg] {
        &self.segs
    }
}

/// A section of Flux's layered-export manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestSection {
    pub name: String,
    pub start_bar: u32,
    pub bars: u32,
    pub start_s: f64,
    pub end_s: f64,
}

/// A layer of Flux's layered-export manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestLayer {
    pub layer: String,
    pub file: String,
    pub lufs: Option<f64>,
    pub peak_dbfs: Option<f64>,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub tracks: Vec<String>,
}

/// A tempo change in Flux's project model: from `bar` on, `bpm` beats per minute.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoPoint {
    pub bar: u32,
    pub bpm: f64,
}

/// The manifest's `tempo`: the project's tempo map (what Flux writes), or one number.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ManifestTempo {
    Constant(f64),
    Map(Vec<TempoPoint>),
}

/// `<stem>.layers.json`, written by Flux's layered export beside the master and its `<stem>_<layer>` files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayersManifest {
    #[serde(default)]
    pub project: Option<String>,
    pub master: Option<String>,
    pub sample_rate: u32,
    pub duration_s: f64,
    #[serde(default)]
    pub looped: bool,
    #[serde(default)]
    pub loop_s: Option<f64>,
    #[serde(default)]
    pub tempo: Option<ManifestTempo>,
    /// `[numerator, denominator]`.
    #[serde(default)]
    pub time_signature: Option<Vec<u32>>,
    #[serde(default)]
    pub bars: Option<u32>,
    /// Length of the first bar, seconds.
    #[serde(default)]
    pub bar_s: Option<f64>,
    #[serde(default)]
    pub sections: Vec<ManifestSection>,
    pub layers: Vec<ManifestLayer>,
    #[serde(default)]
    pub sum_error_db: Option<f64>,
}

impl LayersManifest {
    pub fn read(path: &Path) -> Result<LayersManifest> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing the layers manifest {}", path.display()))
    }

    pub fn sections(&self) -> Vec<Section> {
        self.sections.iter().map(|s| Section::new(s.name.clone(), s.start_s, s.end_s)).collect()
    }

    /// The master file, beside the manifest.
    pub fn master_path(&self, manifest: &Path) -> Option<PathBuf> {
        self.master.as_ref().map(|m| manifest.parent().unwrap_or(Path::new(".")).join(m))
    }

    /// Each layer's file, beside the manifest.
    pub fn layer_paths(&self, manifest: &Path) -> Vec<(String, PathBuf)> {
        let dir = manifest.parent().unwrap_or(Path::new("."));
        self.layers.iter().map(|l| (l.layer.clone(), dir.join(&l.file))).collect()
    }

    /// Quarter notes per bar from the time signature (4 when it is missing or malformed).
    pub fn beats_per_bar(&self) -> f64 {
        match self.time_signature.as_deref() {
            Some([n, d]) if *n > 0 && *d > 0 => *n as f64 * 4.0 / *d as f64,
            _ => 4.0,
        }
    }

    /// The song's bar clock, built like Flux's `TimeMap`: each tempo point starts where the previous tempo
    /// reaches its bar, and a map whose first point is after bar 1 starts at that point's tempo. Without a
    /// `tempo`, the first bar's length (`bar_s`) is used throughout.
    pub fn tempo_map(&self) -> Option<TempoMap> {
        let bpb = self.beats_per_bar();
        let mut pts = match &self.tempo {
            Some(ManifestTempo::Constant(bpm)) => vec![TempoPoint { bar: 1, bpm: *bpm }],
            Some(ManifestTempo::Map(m)) if !m.is_empty() => m.clone(),
            _ => vec![TempoPoint { bar: 1, bpm: bpb * 60.0 / self.bar_s.filter(|s| *s > 0.0)? }],
        };
        pts.sort_by_key(|p| p.bar);
        if pts[0].bar > 1 {
            pts.insert(0, TempoPoint { bar: 1, bpm: pts[0].bpm });
        }
        let mut segs: Vec<TempoSeg> = vec![];
        for p in pts {
            let bar = p.bar.max(1);
            let start_s = match segs.last() {
                Some(prev) => prev.start_s + (bar - prev.bar) as f64 * bpb * 60.0 / prev.bpm,
                None => 0.0,
            };
            segs.push(TempoSeg { bar, bpm: p.bpm, start_s });
        }
        TempoMap::new(segs, bpb).ok()
    }

    /// Sample positions of the bar lines after the first (bars 2..=`bars`), if the manifest has a bar clock.
    pub fn bar_starts(&self) -> Vec<usize> {
        match (self.tempo_map(), self.bars) {
            (Some(tm), Some(bars)) => {
                (2..=bars).map(|b| (tm.bar_time(b) * self.sample_rate as f64).round().max(0.0) as usize).collect()
            }
            _ => vec![],
        }
    }
}

/// A section of the Python pipeline's song metadata (`bars` is "first-last", 1-based and inclusive).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PipelineSection {
    pub name: String,
    pub bars: String,
    #[serde(default)]
    pub start_s: Option<f64>,
}

/// A time stop written into a Temporal Shift piece: the tape stop runs `stop_s..freeze_s`, the music is frozen
/// until `resume_s`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimeStop {
    pub stop_s: f64,
    pub freeze_s: f64,
    pub resume_s: f64,
    #[serde(default)]
    pub frozen_parts: Vec<String>,
    #[serde(default)]
    pub bars: Option<String>,
}

/// The fields of the Python pipeline's `output/<title>/<title>.json` that the analyses use.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PipelineMeta {
    pub title: String,
    #[serde(default)]
    pub piece: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    pub bpm: f64,
    pub bars: u32,
    #[serde(default)]
    pub time_signature: Option<Vec<u32>>,
    #[serde(default)]
    pub r#loop: bool,
    pub loop_length_s: f64,
    #[serde(default)]
    pub sections: Vec<PipelineSection>,
    #[serde(default)]
    pub tempo_map: Option<Vec<TempoSeg>>,
    #[serde(default)]
    pub timestops: Vec<TimeStop>,
}

impl PipelineMeta {
    pub fn read(path: &Path) -> Result<PipelineMeta> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing the song metadata {}", path.display()))
    }

    /// The pipeline's bar clock: its tempo map, or the song tempo throughout. Like the pipeline, every bar is
    /// four quarter notes long.
    pub fn tempo(&self) -> Result<TempoMap> {
        match &self.tempo_map {
            Some(m) if !m.is_empty() => TempoMap::new(m.clone(), 4.0),
            _ => TempoMap::constant(self.bpm, 4.0),
        }
    }

    /// Sample positions of the bar lines after the first (bars 2..=`bars`), rounded like `verify.py`.
    pub fn bar_starts(&self, sr: u32) -> Result<Vec<usize>> {
        let tm = self.tempo()?;
        Ok((2..=self.bars).map(|b| (tm.bar_time(b) * sr as f64).round().max(0.0) as usize).collect())
    }

    /// The song metadata beside an audio file of the pipeline (`<title>.wav`, `<title>_loop.flac`, ...), if there
    /// is one and its loop length matches `len` samples at `sr` (within 1 sample).
    pub fn beside(audio: &Path, sr: u32, len: usize) -> Option<PipelineMeta> {
        let stem = audio.file_stem()?.to_string_lossy().to_string();
        let dir = audio.parent().unwrap_or(Path::new("."));
        let mut names = vec![format!("{stem}.json")];
        if let Some(s) = stem.strip_suffix("_loop") {
            names.push(format!("{s}.json"));
        }
        names.into_iter().find_map(|n| {
            let p = dir.join(n);
            let m = PipelineMeta::read(&p).ok()?;
            ((len as f64 - (m.loop_length_s * sr as f64).round()).abs() <= 1.0).then_some(m)
        })
    }

    /// Sections as time ranges: bars "a-b" run from the start of bar a to the start of bar b + 1. Names are cut at
    /// " - " like the pipeline's tables ("Intro - Silver Pocket Watch" becomes "Intro").
    pub fn sections(&self) -> Result<Vec<Section>> {
        let tm = self.tempo()?;
        self.sections
            .iter()
            .map(|s| {
                let (a, b) = parse_bar_range(&s.bars).with_context(|| format!("section '{}'", s.name))?;
                let name = s.name.split(" - ").next().unwrap_or(&s.name).to_string();
                Ok(Section::new(name, tm.bar_time(a), tm.bar_time(b + 1)))
            })
            .collect()
    }
}

/// "9-24" -> (9, 24); a single bar "5" -> (5, 5).
pub fn parse_bar_range(s: &str) -> Result<(u32, u32)> {
    let parse = |x: &str| x.trim().parse::<u32>().with_context(|| format!("'{x}' in bar range '{s}' is not a bar number"));
    let (a, b) = match s.split_once('-') {
        Some((a, b)) => (parse(a)?, parse(b)?),
        None => {
            let a = parse(s)?;
            (a, a)
        }
    };
    if a == 0 || b < a {
        bail!("bar range '{s}' must be 1-based and increasing");
    }
    Ok((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_map_matches_the_pipeline_bar_clock() {
        let tm = TempoMap::new(
            vec![
                TempoSeg { bar: 1, bpm: 151.0, start_s: 0.0 },
                TempoSeg { bar: 57, bpm: 151.0, start_s: 89.007 },
                TempoSeg { bar: 58, bpm: 157.0, start_s: 90.596 },
            ],
            4.0,
        )
        .unwrap();
        assert_eq!(tm.bar_time(1), 0.0);
        assert!((tm.bar_time(9) - 8.0 * 240.0 / 151.0).abs() < 1e-12);
        assert!((tm.bar_time(59) - (90.596 + 240.0 / 157.0)).abs() < 1e-12);
        assert_eq!(tm.bpm_at(80), 157.0);
        assert!(TempoMap::constant(0.0, 4.0).is_err());
        assert!(TempoMap::new(vec![TempoSeg { bar: 3, bpm: 100.0, start_s: 0.0 }], 4.0).is_err());
    }

    #[test]
    fn bar_ranges_and_section_samples() {
        assert_eq!(parse_bar_range("9-24").unwrap(), (9, 24));
        assert_eq!(parse_bar_range(" 5 ").unwrap(), (5, 5));
        assert!(parse_bar_range("0-3").is_err());
        assert!(parse_bar_range("8-2").is_err());
        assert!(parse_bar_range("a-b").is_err());
        let s = Section::new("A", 0.5, 2.0);
        assert_eq!(s.samples(10, 100), (5, 20));
        assert_eq!(s.samples(10, 12), (5, 12));
        assert!(check_sections(std::slice::from_ref(&s), 3.0).is_ok());
        assert!(check_sections(&[s], 0.4).is_err());
        assert!(check_sections(&[Section::new("B", 2.0, 1.0)], 3.0).is_err());
    }

    #[test]
    fn manifests_parse() {
        let m: LayersManifest = serde_json::from_str(
            r#"{"project":"p","master":"song.flac","sample_rate":48000,"duration_s":8.0,"looped":true,"loop_s":8.0,
            "tempo":120,"time_signature":[4,4],"bars":4,"bar_s":2.0,
            "sections":[{"name":"A","start_bar":1,"bars":4,"start_s":0.0,"end_s":8.0}],
            "layers":[{"layer":"base","file":"song_base.flac","lufs":-16.0,"peak_dbfs":-2.0,"bytes":10,"tracks":["x"]}],
            "sum_error_db":-120.0,"usage":"..."}"#,
        )
        .unwrap();
        assert_eq!(m.sections()[0], Section::new("A", 0.0, 8.0));
        assert_eq!(m.bar_starts(), vec![96000, 192000, 288000]);
        // what Flux writes: the project's tempo map, here 120 BPM then 60 BPM from bar 3, in 3/4
        let mut m2 = m.clone();
        m2.tempo = Some(ManifestTempo::Map(vec![TempoPoint { bar: 3, bpm: 60.0 }, TempoPoint { bar: 1, bpm: 120.0 }]));
        m2.time_signature = Some(vec![3, 4]);
        assert_eq!(m2.bar_starts(), vec![72000, 144000, 288000]);
        let flux: LayersManifest = serde_json::from_str(
            r#"{"bar_s":2.4,"bars":4,"duration_s":9.6,"layers":[{"bytes":1,"file":"mixed_pads.wav","layer":"pads",
            "lufs":-15.0,"peak_dbfs":-2.7,"tracks":["Pad","Keys"]}],"loop_s":9.6,"looped":true,"master":"mixed.wav",
            "project":"t","sample_rate":44100,"sections":[],"sum_error_db":-240.0,"tempo":[{"bar":1,"bpm":100.0}],
            "time_signature":[4,4],"usage":"..."}"#,
        )
        .unwrap();
        assert_eq!(flux.bar_starts(), vec![105840, 211680, 317520]);
        // no tempo: the first bar's length throughout
        let mut m3 = m.clone();
        m3.tempo = None;
        assert_eq!(m3.bar_starts(), vec![96000, 192000, 288000]);
        m3.bar_s = None;
        assert!(m3.bar_starts().is_empty());
        assert_eq!(m.master_path(Path::new("d/song.layers.json")).unwrap(), Path::new("d").join("song.flac"));
        assert_eq!(m.layer_paths(Path::new("song.layers.json"))[0].1, Path::new("").join("song_base.flac"));
        let p: PipelineMeta = serde_json::from_str(
            r#"{"title":"t","bpm":120,"bars":8,"loop":true,"loop_length_s":16.0,
            "sections":[{"name":"Intro - Dawn","bars":"1-4","start_s":0.0},{"name":"B","bars":"5-8"}]}"#,
        )
        .unwrap();
        let s = p.sections().unwrap();
        assert_eq!(s[0], Section::new("Intro", 0.0, 8.0));
        assert_eq!(s[1], Section::new("B", 8.0, 16.0));
    }

    #[test]
    fn pipeline_metadata_beside_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let meta = r#"{"title":"far_roads","bpm":120,"bars":4,"loop":true,"loop_length_s":8.0,"sections":[],
            "tempo_map":[{"bar":1,"bpm":120.0,"start_s":0.0},{"bar":3,"bpm":60.0,"start_s":4.0}]}"#;
        std::fs::write(dir.path().join("far_roads.json"), meta).unwrap();
        let m = PipelineMeta::read(&dir.path().join("far_roads.json")).unwrap();
        // bars 2..=4: 2 s at 120 BPM, then 4 s per bar at 60 BPM from bar 3
        assert_eq!(m.bar_starts(1000).unwrap(), vec![2000, 4000, 8000]);
        // the loop file and the plain file both find it, when the length matches within one sample
        for name in ["far_roads.wav", "far_roads_loop.flac"] {
            let p = dir.path().join(name);
            assert!(PipelineMeta::beside(&p, 1000, 8000).is_some(), "{name}");
            assert!(PipelineMeta::beside(&p, 1000, 8001).is_some(), "{name}");
            assert!(PipelineMeta::beside(&p, 1000, 8002).is_none(), "{name}: another length is another song");
        }
        assert!(PipelineMeta::beside(&dir.path().join("other.wav"), 1000, 8000).is_none());
        std::fs::write(dir.path().join("broken.json"), "{").unwrap();
        assert!(PipelineMeta::beside(&dir.path().join("broken.wav"), 1000, 8000).is_none());
        assert!(PipelineMeta::read(&dir.path().join("broken.json")).is_err());
    }
}
