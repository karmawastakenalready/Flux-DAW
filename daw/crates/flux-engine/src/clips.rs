//! Audio clips: their files read (WAV, FLAC, Ogg Vorbis), resampled to the project's rate and kept in a cache, mixed
//! onto an audio track's timeline with their trims, gains and fades; and their waveforms, for drawing.

use crate::dsp::Stereo;
use crate::{paths, resample};
use anyhow::{anyhow, bail, ensure, Context, Result};
use flux_core::{Clip, Project, TimeMap, Track};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Decoded audio kept for clips, at most this many bytes (the least recently used goes first).
const CACHE_BYTES: usize = 1 << 30;
/// Frames of a file per waveform column.
pub const PEAK_FRAMES: usize = 256;

/// What a file holds, from its header.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

impl FileInfo {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate.max(1) as f64
    }
}

/// Which version of a file: its modification time and size.
type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Result<Stamp> {
    let m = std::fs::metadata(path).map_err(|e| anyhow!("{}: {}", path.display(), if e.kind() == std::io::ErrorKind::NotFound { "no such file".to_string() } else { e.to_string() }))?;
    Ok((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Wav,
    Flac,
    Ogg,
}

fn format(path: &Path) -> Result<Format> {
    let mut magic = [0u8; 4];
    File::open(path)?.read_exact(&mut magic).map_err(|_| anyhow!("{}: too short to be audio", path.display()))?;
    match &magic {
        b"RIFF" => Ok(Format::Wav),
        b"fLaC" => Ok(Format::Flac),
        b"OggS" => Ok(Format::Ogg),
        _ => bail!("{}: not a WAV, FLAC or Ogg Vorbis file", path.display()),
    }
}

/// The last Ogg page's granule position: how many frames an Ogg Vorbis stream holds.
fn ogg_frames(path: &Path) -> Result<Option<u64>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let back = len.min(1 << 16);
    f.seek(SeekFrom::Start(len - back))?;
    let mut buf = vec![0u8; back as usize];
    f.read_exact(&mut buf)?;
    let at = buf.windows(4).rposition(|w| w == b"OggS");
    Ok(at.filter(|&i| i + 14 <= buf.len()).map(|i| u64::from_le_bytes(buf[i + 6..i + 14].try_into().unwrap())))
}

fn read_info(path: &Path) -> Result<FileInfo> {
    let info = match format(path)? {
        Format::Wav => {
            let r = hound::WavReader::open(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
            let s = r.spec();
            FileInfo { sample_rate: s.sample_rate, channels: s.channels, frames: r.duration() as u64 }
        }
        Format::Flac => {
            let r = claxon::FlacReader::open(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
            let s = r.streaminfo();
            let frames = match s.samples {
                Some(n) => n,
                None => decode(path)?.1 .0.len() as u64,
            };
            FileInfo { sample_rate: s.sample_rate, channels: s.channels as u16, frames }
        }
        Format::Ogg => {
            let r = lewton::inside_ogg::OggStreamReader::new(BufReader::new(File::open(path)?)).map_err(|e| anyhow!("{}: {e}", path.display()))?;
            let frames = match ogg_frames(path)? {
                Some(n) => n,
                None => decode(path)?.1 .0.len() as u64,
            };
            FileInfo { sample_rate: r.ident_hdr.audio_sample_rate, channels: r.ident_hdr.audio_channels as u16, frames }
        }
    };
    ensure!(info.sample_rate > 0 && info.channels > 0, "{}: no audio in it", path.display());
    Ok(info)
}

/// The two channels of interleaved audio: mono plays in both; past two channels, the first two.
fn stereo_of(samples: impl Iterator<Item = f32>, ch: usize, cap: usize) -> Stereo {
    let (mut l, mut r) = (Vec::with_capacity(cap), Vec::with_capacity(cap));
    for (i, v) in samples.enumerate() {
        match i % ch {
            0 => {
                l.push(v);
                if ch == 1 {
                    r.push(v);
                }
            }
            1 => r.push(v),
            _ => {}
        }
    }
    r.truncate(l.len());
    (l, r)
}

/// The whole file, at its own rate.
fn decode(path: &Path) -> Result<(u32, Stereo)> {
    let fail = |e: &dyn std::fmt::Display| anyhow!("{}: {e}", path.display());
    match format(path)? {
        Format::Wav => {
            let mut r = hound::WavReader::open(path).map_err(|e| fail(&e))?;
            let s = r.spec();
            let ch = s.channels.max(1) as usize;
            let cap = r.duration() as usize;
            let st = match s.sample_format {
                hound::SampleFormat::Float => {
                    ensure!(s.bits_per_sample == 32, "{}: {}-bit float WAV isn't supported", path.display(), s.bits_per_sample);
                    let v: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().map_err(|e| fail(&e))?;
                    stereo_of(v.into_iter(), ch, cap)
                }
                hound::SampleFormat::Int => {
                    ensure!((8..=32).contains(&s.bits_per_sample), "{}: {}-bit PCM isn't supported", path.display(), s.bits_per_sample);
                    let scale = 1.0 / (1u64 << (s.bits_per_sample - 1)) as f64;
                    let v: Vec<i32> = r.samples::<i32>().collect::<Result<_, _>>().map_err(|e| fail(&e))?;
                    stereo_of(v.into_iter().map(|x| (x as f64 * scale) as f32), ch, cap)
                }
            };
            Ok((s.sample_rate, st))
        }
        Format::Flac => {
            let mut r = claxon::FlacReader::open(path).map_err(|e| fail(&e))?;
            let s = r.streaminfo();
            let scale = 1.0 / (1u64 << (s.bits_per_sample - 1)) as f64;
            let cap = s.samples.unwrap_or(0) as usize;
            let (mut l, mut rr) = (Vec::with_capacity(cap), Vec::with_capacity(cap));
            let mut blocks = r.blocks();
            let mut buf = Vec::new();
            while let Some(b) = blocks.read_next_or_eof(buf).map_err(|e| fail(&e))? {
                let a = b.channel(0);
                let c = if s.channels > 1 { b.channel(1) } else { a };
                l.extend(a.iter().map(|&v| (v as f64 * scale) as f32));
                rr.extend(c.iter().map(|&v| (v as f64 * scale) as f32));
                buf = b.into_buffer();
            }
            Ok((s.sample_rate, (l, rr)))
        }
        Format::Ogg => {
            let mut r = lewton::inside_ogg::OggStreamReader::new(BufReader::new(File::open(path)?)).map_err(|e| fail(&e))?;
            let ch = r.ident_hdr.audio_channels.max(1) as usize;
            let (mut l, mut rr) = (vec![], vec![]);
            while let Some(p) = r.read_dec_packet_generic::<Vec<Vec<f32>>>().map_err(|e| fail(&e))? {
                if let Some(a) = p.first() {
                    l.extend_from_slice(a);
                    rr.extend_from_slice(if ch > 1 { &p[1] } else { a });
                }
            }
            // the stream ends where its last granule position says (the decoder only trims it when the audio
            // spans more than one page)
            if let Some(n) = ogg_frames(path)? {
                l.truncate(n as usize);
                rr.truncate(n as usize);
            }
            Ok((r.ident_hdr.audio_sample_rate, (l, rr)))
        }
    }
}

// ------------------------------------------------------------------ caches

struct Caches {
    info: HashMap<PathBuf, (Stamp, FileInfo)>,
    /// (file, version, rate) -> the file at that rate, most recently used last.
    audio: Vec<((PathBuf, Stamp, u32), Arc<Stereo>)>,
    peaks: HashMap<PathBuf, PeakState>,
}

enum PeakState {
    Loading,
    Ready { stamp: Stamp, checked: Instant, peaks: Arc<Peaks> },
    /// `stamp`: the file as it was then (None: there was none), so it is read again only once it changes.
    Failed { stamp: Option<Stamp>, checked: Instant, error: String },
}

fn caches() -> &'static Mutex<Caches> {
    static C: OnceLock<Mutex<Caches>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Caches { info: HashMap::new(), audio: vec![], peaks: HashMap::new() }))
}

fn lock() -> std::sync::MutexGuard<'static, Caches> {
    caches().lock().unwrap_or_else(|e| e.into_inner())
}

/// What the file holds (from its header; cached until it changes).
pub fn info(path: &Path) -> Result<FileInfo> {
    let st = stamp(path)?;
    if let Some((s, i)) = lock().info.get(path)
        && *s == st
    {
        return Ok(*i);
    }
    let i = read_info(path)?;
    lock().info.insert(path.to_path_buf(), (st, i));
    Ok(i)
}

fn cached(key: &(PathBuf, Stamp, u32)) -> Option<Arc<Stereo>> {
    let mut c = lock();
    let i = c.audio.iter().position(|e| e.0 == *key)?;
    let e = c.audio.remove(i);
    let a = e.1.clone();
    c.audio.push(e);
    Some(a)
}

fn keep(key: (PathBuf, Stamp, u32), a: Arc<Stereo>) {
    let mut c = lock();
    c.audio.retain(|e| e.0 != key);
    c.audio.push((key, a));
    let bytes = |c: &Caches| c.audio.iter().map(|e| (e.1 .0.len() + e.1 .1.len()) * 4).sum::<usize>();
    while c.audio.len() > 1 && bytes(&c) > CACHE_BYTES {
        c.audio.remove(0);
    }
}

/// The file decoded at its own rate.
fn decoded(path: &Path, st: Stamp) -> Result<(u32, Arc<Stereo>)> {
    let own = info(path)?.sample_rate;
    if let Some(a) = cached(&(path.to_path_buf(), st, own)) {
        return Ok((own, a));
    }
    let (sr, a) = decode(path)?;
    let a = Arc::new(a);
    keep((path.to_path_buf(), st, sr), a.clone());
    Ok((sr, a))
}

/// The file's audio at `sr` Hz (resampled when it is at another rate; cached).
pub fn source(path: &Path, sr: u32) -> Result<Arc<Stereo>> {
    let st = stamp(path)?;
    let key = (path.to_path_buf(), st, sr);
    if let Some(a) = cached(&key) {
        return Ok(a);
    }
    let (own, a) = decoded(path, st)?;
    if own == sr {
        return Ok(a);
    }
    let (l, r) = std::thread::scope(|s| {
        let l = s.spawn(|| resample::resample(&a.0, own, sr));
        let r = resample::resample(&a.1, own, sr);
        (l.join().unwrap_or_default(), r)
    });
    let out = Arc::new((l, r));
    keep(key, out.clone());
    Ok(out)
}

// ------------------------------------------------------------------ waveforms

/// A file's waveform for drawing: the lowest and highest sample (of its channels' mean) in each [`PEAK_FRAMES`]
/// frames, at its own rate.
#[derive(Debug, PartialEq)]
pub struct Peaks {
    pub sample_rate: u32,
    pub frames: u64,
    pub cols: Vec<(f32, f32)>,
}

impl Peaks {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate.max(1) as f64
    }

    /// The lowest and highest sample between `a` and `b` seconds into the file (None outside it).
    pub fn range(&self, a: f64, b: f64) -> Option<(f32, f32)> {
        let per = PEAK_FRAMES as f64 / self.sample_rate.max(1) as f64;
        let i0 = (a.max(0.0) / per).floor() as usize;
        let i1 = ((b / per).ceil() as usize).min(self.cols.len());
        (i0 < i1).then(|| self.cols[i0..i1].iter().fold((f32::MAX, f32::MIN), |(lo, hi), c| (lo.min(c.0), hi.max(c.1))))
    }
}

fn peaks_of(sr: u32, a: &Stereo) -> Peaks {
    let cols = a
        .0
        .chunks(PEAK_FRAMES)
        .zip(a.1.chunks(PEAK_FRAMES))
        .map(|(l, r)| l.iter().zip(r).map(|(x, y)| (x + y) * 0.5).fold((f32::MAX, f32::MIN), |(lo, hi), m| (lo.min(m), hi.max(m))))
        .collect();
    Peaks { sample_rate: sr, frames: a.0.len() as u64, cols }
}

/// The file's waveform when it is ready (or why it can't be read); else it is read in the background, and `ready`
/// called once it is. A file is looked at again for changes at most once a second, and read again when it has
/// changed (or appeared).
pub fn peaks(path: &Path, ready: impl FnOnce() + Send + 'static) -> Option<Result<Arc<Peaks>, String>> {
    const RECHECK: Duration = Duration::from_secs(1);
    {
        let mut c = lock();
        match c.peaks.get_mut(path) {
            Some(PeakState::Loading) => return None,
            Some(PeakState::Ready { stamp: s, checked, peaks }) => {
                if checked.elapsed() < RECHECK {
                    return Some(Ok(peaks.clone()));
                }
                *checked = Instant::now();
                let s = *s;
                let p = peaks.clone();
                drop(c);
                if stamp(path).ok() == Some(s) {
                    return Some(Ok(p));
                }
            }
            Some(PeakState::Failed { stamp: s, checked, error }) => {
                if checked.elapsed() < RECHECK {
                    return Some(Err(error.clone()));
                }
                *checked = Instant::now();
                let (s, e) = (*s, error.clone());
                drop(c);
                if stamp(path).ok() == s {
                    return Some(Err(e));
                }
            }
            None => {}
        }
    }
    lock().peaks.insert(path.to_path_buf(), PeakState::Loading);
    let key = path.to_path_buf();
    let path = path.to_path_buf();
    let spawned = std::thread::Builder::new().name("clip-peaks".into()).spawn(move || {
        let got = stamp(&path).map(|st| (st, decoded(&path, st).map(|(sr, a)| peaks_of(sr, &a))));
        let state = match got {
            Ok((stamp, Ok(p))) => PeakState::Ready { stamp, checked: Instant::now(), peaks: Arc::new(p) },
            Ok((stamp, Err(e))) => PeakState::Failed { stamp: Some(stamp), checked: Instant::now(), error: format!("{e:#}") },
            Err(e) => PeakState::Failed { stamp: None, checked: Instant::now(), error: format!("{e:#}") },
        };
        lock().peaks.insert(path, state);
        ready();
    });
    if spawned.is_err() {
        lock().peaks.remove(&key);
    }
    None
}

// ------------------------------------------------------------------ playing

/// A track's clips mixed at the project's rate, `frames` long (what runs past the end is cut).
pub fn render(track: &Track, p: &Project, tm: &TimeMap, frames: usize) -> Result<Stereo> {
    let sr = p.sample_rate;
    let mut out = (vec![0f32; frames], vec![0f32; frames]);
    for c in &track.clips {
        let src = source(&paths::resolve(&c.path), sr).with_context(|| format!("clip #{} ({})", c.id, c.file_name()))?;
        mix(c, &src, sr, tm, &mut out);
    }
    Ok(out)
}

/// One clip into `out`: its stretch of `src` (the file at `sr`), with its gain and fades.
fn mix(c: &Clip, src: &Stereo, sr: u32, tm: &TimeMap, out: &mut Stereo) {
    let srf = sr as f64;
    let first = (c.offset * srf).round() as usize;
    let avail = src.0.len().saturating_sub(first);
    let len = c.length.map_or(avail, |l| ((l * srf).round() as usize).min(avail));
    let at = (tm.sec(c.start) * srf).round() as usize;
    // the fades shape the audio there is
    let (fi, fo) = c.fades(len as f64 / srf);
    let (fi, fo) = ((fi * srf).round() as usize, (fo * srf).round() as usize);
    let g = 10f64.powf(c.gain_db / 20.0);
    let n = len.min(out.0.len().saturating_sub(at));
    for k in 0..n {
        let mut v = g;
        if k < fi {
            v *= k as f64 / fi as f64;
        }
        if k + fo > len {
            v *= (len - k) as f64 / fo as f64;
        }
        let v = v as f32;
        out.0[at + k] += src.0[first + k] * v;
        out.1[at + k] += src.1[first + k] * v;
    }
}

/// The files a track's clips play.
pub fn files(track: &Track) -> Vec<PathBuf> {
    track.clips.iter().map(|c| paths::resolve(&c.path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, sr: u32, ch: u16, frames: &[(f32, f32)]) {
        let spec = hound::WavSpec { channels: ch, sample_rate: sr, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &(l, r) in frames {
            w.write_sample((l * 32767.0) as i16).unwrap();
            if ch == 2 {
                w.write_sample((r * 32767.0) as i16).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flux-clips-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn info_and_peaks() {
        let d = temp("info");
        let f = d.join("ramp.wav");
        let frames: Vec<(f32, f32)> = (0..1000).map(|i| (i as f32 / 1000.0, -(i as f32) / 1000.0)).collect();
        write_wav(&f, 22050, 2, &frames);
        assert_eq!(info(&f).unwrap(), FileInfo { sample_rate: 22050, channels: 2, frames: 1000 });
        let mono = d.join("mono.wav");
        write_wav(&mono, 44100, 1, &[(0.5, 0.0); 300]);
        let (_, st) = decode(&mono).unwrap();
        assert_eq!((st.0.len(), st.1.len()), (300, 300));
        assert!(st.1.iter().all(|v| (*v - 0.5).abs() < 1e-3), "mono plays in both channels");
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(peaks(&mono, move || tx.send(()).unwrap()).is_none(), "read in the background");
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let p = peaks(&mono, || {}).unwrap().unwrap();
        assert_eq!((p.frames, p.cols.len()), (300, 2));
        assert!((p.range(0.0, 1.0).unwrap().1 - 0.5).abs() < 1e-3);
        assert!(p.range(1.0, 2.0).is_none());
        let bad = d.join("text.wav");
        std::fs::write(&bad, "not audio").unwrap();
        assert!(format!("{:#}", info(&bad).unwrap_err()).contains("not a WAV"));
        assert!(format!("{:#}", info(&d.join("gone.wav")).unwrap_err()).contains("no such file"));
    }

    #[test]
    fn clips_play_where_they_are_put() {
        let d = temp("render");
        let f = d.join("dc.wav");
        // 1 s of a constant at 8 kHz
        write_wav(&f, 8000, 2, &[(0.5, 0.25); 8000]);
        // 120 bpm: a beat is half a second
        let mut p = Project::new("t", 120.0, 4);
        p.sample_rate = 8000;
        let tm = TimeMap::new(&p);
        let mut t: Track = serde_json::from_value(serde_json::json!({"id": 1, "name": "fx", "instrument": {"kind": "audio"}})).unwrap();
        t.clips = vec![
            Clip { offset: 0.25, length: Some(0.5), gain_db: -6.0206, fade_in: 0.1, ..Clip::new(2, f.to_string_lossy(), 2.0) },
            Clip { fade_out: 0.5, ..Clip::new(3, f.to_string_lossy(), 6.0) },
        ];
        let (l, r) = render(&t, &p, &tm, 8 * 8000).unwrap();
        // the first: beat 2 (1 s) for half a second at half the level, rising over 0.1 s
        let at = |s: f64| (s * 8000.0).round() as usize;
        assert_eq!(l[at(1.0) - 1], 0.0);
        assert!(l[at(1.0)].abs() < 1e-6);
        assert!((l[at(1.05)] - 0.125).abs() < 2e-3, "{}", l[at(1.05)]);
        assert!((l[at(1.3)] - 0.25).abs() < 1e-3 && (r[at(1.3)] - 0.125).abs() < 1e-3);
        assert_eq!(l[at(1.5)], 0.0, "then stops");
        // the second: beat 6 (3 s) to the end of the file, fading out over its last half second
        assert!((l[at(3.25)] - 0.5).abs() < 1e-3);
        assert!((l[at(3.75)] - 0.25).abs() < 2e-3);
        assert!(l[at(4.0)..].iter().all(|v| *v == 0.0));
        // a missing file names the clip
        t.clips.push(Clip::new(9, "nowhere/gone.wav", 0.0));
        let e = format!("{:#}", render(&t, &p, &tm, 100).unwrap_err());
        assert!(e.contains("clip #9 (gone.wav)") && e.contains("no such file"), "{e}");
    }

    #[test]
    fn ogg_vorbis() {
        // a quarter second of 440 Hz at half scale on the left, silence on the right (written by libsndfile)
        let f = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("tone.ogg");
        assert_eq!(info(&f).unwrap(), FileInfo { sample_rate: 44100, channels: 2, frames: 11025 });
        let (sr, (l, r)) = decode(&f).unwrap();
        assert_eq!((sr, l.len(), r.len()), (44100, 11025, 11025));
        let rms = |x: &[f32]| (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        assert!((rms(&l[2000..9000]) - 0.5 / 2f64.sqrt()).abs() < 0.01, "{}", rms(&l[2000..9000]));
        assert!(rms(&r) < 1e-3);
        // a cycle is 44100 / 440 frames: count the upward zero crossings
        let ups = l[2000..9000].windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((69..=71).contains(&ups), "{ups}");
        // in time: it starts where the tone did
        let ideal = |i: usize| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() as f32 * 0.5;
        let err = (1000..10000).map(|i| (l[i] - ideal(i)).abs()).fold(0f32, f32::max);
        assert!(err < 0.02, "{err}");
    }

    #[test]
    fn files_at_another_rate_are_resampled() {
        let d = temp("rate");
        let f = d.join("tone.wav");
        let tone: Vec<(f32, f32)> = (0..48000).map(|i| ((2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48000.0).sin() as f32 * 0.5, 0.0)).collect();
        write_wav(&f, 48000, 2, &tone);
        let a = source(&f, 44100).unwrap();
        assert_eq!(a.0.len(), 44100);
        let mid = 22050;
        let want = (2.0 * std::f64::consts::PI * 440.0 * mid as f64 / 44100.0).sin() as f32 * 0.5;
        assert!((a.0[mid] - want).abs() < 1e-3, "{} vs {want}", a.0[mid]);
        // the same again comes from the cache
        assert!(Arc::ptr_eq(&a, &source(&f, 44100).unwrap()));
    }
}
