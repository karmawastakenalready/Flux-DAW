//! Sample files: header information (format, length, root key, loops) and decoding to floating point.
//!
//! WAV (RIFF and RF64; PCM 8/16/24/32-bit, IEEE float 32/64-bit, WAVE_FORMAT_EXTENSIBLE) is read by this
//! module; FLAC headers are read here and the audio is decoded with `claxon`. Loops and the root key come from
//! the `smpl` chunk, which FLAC files can carry in a `riff` application block (as written by
//! `flac --keep-foreign-metadata`); sfizz reads both. Headers are read without loading the audio, so
//! inspecting a library of thousands of samples stays fast.
//!
//! Decoded values are scaled like libsndfile does (16-bit by 1/32768, 24-bit by 1/8388608...), so analyses
//! match the Python tools that read samples through `soundfile`.

use crate::error::{Error, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The container of a sample file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Wav,
    Flac,
}

/// One loop of a sample, in frames. `end` is inclusive, as stored in the `smpl` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SampleLoop {
    pub start: u64,
    pub end: u64,
    /// 0 forward, 1 alternating, 2 backward (smpl loop type).
    pub kind: u32,
    /// 0 means infinite.
    pub play_count: u32,
}

/// What a sample file's headers say.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SampleInfo {
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// True for IEEE floating-point samples.
    pub float: bool,
    pub frames: u64,
    pub file_size: u64,
    /// MIDI root key from the `smpl` chunk (`pitch_keycenter=sample` uses it).
    pub root_key: Option<u8>,
    /// Pitch fraction of the root key from the `smpl` chunk, in cents.
    pub root_fine_cents: f64,
    pub loops: Vec<SampleLoop>,
}

impl SampleInfo {
    /// Read the headers of a WAV or FLAC file (the format is detected from the content, not the extension).
    pub fn read(path: &Path) -> Result<SampleInfo> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        let file_size = file.metadata().map_err(|e| Error::io(path, e))?.len();
        let mut r = BufReader::new(file);
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic).map_err(|e| Error::audio(path, format!("too short to be audio ({e})")))?;
        r.seek(SeekFrom::Start(0)).map_err(|e| Error::io(path, e))?;
        let mut info = match &magic {
            b"RIFF" | b"RF64" => wav_header(&mut r, path)?.info,
            b"fLaC" => flac_header(&mut r, path)?,
            b"RIFX" => return Err(Error::audio(path, "big-endian WAV (RIFX) is not supported")),
            _ => return Err(Error::audio(path, "not a WAV or FLAC file")),
        };
        info.file_size = file_size;
        Ok(info)
    }

    /// Length in seconds.
    pub fn duration(&self) -> f64 {
        if self.sample_rate == 0 { 0.0 } else { self.frames as f64 / f64::from(self.sample_rate) }
    }

    /// `"PCM_16"`, `"PCM_24"`, `"FLOAT"`... (libsndfile's subtype names).
    pub fn encoding(&self) -> String {
        match (self.float, self.bits_per_sample) {
            (true, 64) => "DOUBLE".into(),
            (true, _) => "FLOAT".into(),
            (false, 8) if self.format == AudioFormat::Wav => "PCM_U8".into(),
            (false, b) => format!("PCM_{b}"),
        }
    }
}

/// Decoded audio: interleaved samples scaled to -1..1.
#[derive(Clone, Debug, PartialEq)]
pub struct Audio {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f64>,
}

impl Audio {
    /// Decode a whole WAV or FLAC file.
    pub fn read(path: &Path) -> Result<Audio> {
        Audio::read_frames(path, u64::MAX)
    }

    /// Decode at most `max_frames` frames from the start of the file.
    pub fn read_frames(path: &Path, max_frames: u64) -> Result<Audio> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        let mut r = BufReader::new(file);
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic).map_err(|e| Error::audio(path, format!("too short to be audio ({e})")))?;
        r.seek(SeekFrom::Start(0)).map_err(|e| Error::io(path, e))?;
        match &magic {
            b"RIFF" | b"RF64" => {
                let h = wav_header(&mut r, path)?;
                decode_wav(&mut r, path, &h, max_frames)
            }
            b"fLaC" => decode_flac(path, max_frames),
            _ => Err(Error::audio(path, "not a WAV or FLAC file")),
        }
    }

    pub fn frames(&self) -> usize {
        if self.channels == 0 { 0 } else { self.samples.len() / usize::from(self.channels) }
    }

    /// The mean of the channels (what the Python tools analyse: `x.mean(axis=1)`).
    pub fn mono(&self) -> Vec<f64> {
        let ch = usize::from(self.channels.max(1));
        self.samples.chunks_exact(ch).map(|f| f.iter().sum::<f64>() / ch as f64).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// WAV

struct WavHeader {
    info: SampleInfo,
    data_offset: u64,
    data_len: u64,
    block_align: u16,
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn read_u64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8])))
}

/// Parse a `smpl` chunk body: root key, pitch fraction and loops.
fn parse_smpl(body: &[u8]) -> (Option<u8>, f64, Vec<SampleLoop>) {
    if body.len() < 36 {
        return (None, 0.0, vec![]);
    }
    let root = read_u32(body, 12);
    let fraction = read_u32(body, 16);
    let n = read_u32(body, 28) as usize;
    let loops = (0..n)
        .map_while(|i| {
            let at = 36 + i * 24;
            (at + 24 <= body.len()).then(|| SampleLoop {
                kind: read_u32(body, at + 4),
                start: u64::from(read_u32(body, at + 8)),
                end: u64::from(read_u32(body, at + 12)),
                play_count: read_u32(body, at + 20),
            })
        })
        .collect();
    let root_key = u8::try_from(root).ok().filter(|k| *k <= 127);
    (root_key, f64::from(fraction) / 4_294_967_296.0 * 100.0, loops)
}

fn wav_header<R: Read + Seek>(r: &mut R, path: &Path) -> Result<WavHeader> {
    let bad = |m: &str| Error::audio(path, m.to_owned());
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|_| bad("truncated RIFF header"))?;
    if &head[8..12] != b"WAVE" {
        return Err(bad("RIFF file is not WAVE"));
    }
    let rf64 = &head[0..4] == b"RF64";
    let mut ds64_data: Option<u64> = None;
    let mut fmt: Option<Vec<u8>> = None;
    let mut data: Option<(u64, u64)> = None;
    let mut smpl: Option<Vec<u8>> = None;
    let mut pos = 12u64;
    loop {
        let mut ch = [0u8; 8];
        if r.read_exact(&mut ch).is_err() {
            break;
        }
        let id = [ch[0], ch[1], ch[2], ch[3]];
        let mut size = u64::from(read_u32(&ch, 4));
        let body_at = pos + 8;
        match &id {
            b"fmt " | b"smpl" | b"ds64" => {
                let len = size.min(1 << 16) as usize;
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).map_err(|_| bad("truncated chunk"))?;
                match &id {
                    b"fmt " => fmt = Some(body),
                    b"smpl" => smpl = Some(body),
                    _ => ds64_data = Some(read_u64(&body, 8)),
                }
            }
            b"data" => {
                if rf64 && size == 0xFFFF_FFFF {
                    // the real size is in the ds64 chunk
                    size = ds64_data.unwrap_or(0);
                }
                data = Some((body_at, size));
            }
            _ => {}
        }
        // chunks are padded to an even size
        pos = body_at + size + (size & 1);
        if r.seek(SeekFrom::Start(pos)).is_err() {
            break;
        }
    }
    let fmt = fmt.ok_or_else(|| bad("no fmt chunk"))?;
    let (data_offset, data_len) = data.ok_or_else(|| bad("no data chunk"))?;
    let end = r.seek(SeekFrom::End(0)).map_err(|e| Error::io(path, e))?;
    // a data size past the end of the file (truncated or streamed file): use what is there
    let data_len = data_len.min(end.saturating_sub(data_offset));
    let mut tag = read_u16(&fmt, 0);
    let channels = read_u16(&fmt, 2);
    let sample_rate = read_u32(&fmt, 4);
    let block_align = read_u16(&fmt, 12);
    let bits = read_u16(&fmt, 14);
    if tag == 0xFFFE && fmt.len() >= 26 {
        tag = read_u16(&fmt, 24);
    }
    let float = match tag {
        1 => false,
        3 => true,
        other => return Err(bad(&format!("unsupported WAV encoding (format tag {other:#x})"))),
    };
    let supported = if float { matches!(bits, 32 | 64) } else { matches!(bits, 8 | 16 | 24 | 32) };
    // block_align may exceed channels * width (padded containers) but never be smaller
    if channels == 0 || !supported || u32::from(block_align) < u32::from(channels) * u32::from(bits / 8) {
        return Err(bad(&format!(
            "unsupported WAV layout ({channels} channels, {bits}-bit {}, {block_align} bytes per frame)",
            if float { "float" } else { "integer" }
        )));
    }
    let (root_key, root_fine_cents, loops) = smpl.as_deref().map(parse_smpl).unwrap_or((None, 0.0, vec![]));
    Ok(WavHeader {
        info: SampleInfo {
            format: AudioFormat::Wav,
            sample_rate,
            channels,
            bits_per_sample: bits,
            float,
            frames: data_len / u64::from(block_align),
            file_size: 0,
            root_key,
            root_fine_cents,
            loops,
        },
        data_offset,
        data_len,
        block_align,
    })
}

fn decode_wav<R: Read + Seek>(r: &mut R, path: &Path, h: &WavHeader, max_frames: u64) -> Result<Audio> {
    let frames = h.info.frames.min(max_frames);
    let len = usize::try_from(frames * u64::from(h.block_align)).map_err(|_| Error::audio(path, "file too large"))?;
    let len = len.min(h.data_len as usize);
    r.seek(SeekFrom::Start(h.data_offset)).map_err(|e| Error::io(path, e))?;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes).map_err(|e| Error::io(path, e))?;
    let i = &h.info;
    let width = usize::from(i.bits_per_sample / 8);
    let channels = usize::from(i.channels);
    let frame_bytes = usize::from(h.block_align);
    // block_align can exceed channels * width (padded containers); read each channel at its offset
    let mut samples = Vec::with_capacity(frames as usize * channels);
    for frame in bytes.chunks_exact(frame_bytes) {
        for c in 0..channels {
            let s = &frame[c * width..(c + 1) * width];
            let v = match (i.float, width) {
                (true, 4) => f64::from(f32::from_le_bytes([s[0], s[1], s[2], s[3]])),
                (true, 8) => f64::from_le_bytes(s.try_into().unwrap_or([0; 8])),
                (false, 1) => (f64::from(s[0]) - 128.0) / 128.0,
                (false, 2) => f64::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0,
                (false, 3) => f64::from(i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) / 8_388_608.0,
                (false, 4) => f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2_147_483_648.0,
                _ => return Err(Error::audio(path, "unsupported sample width")),
            };
            samples.push(v);
        }
    }
    Ok(Audio { sample_rate: i.sample_rate, channels: i.channels, samples })
}

// ---------------------------------------------------------------------------------------------
// FLAC

fn flac_header<R: Read>(r: &mut R, path: &Path) -> Result<SampleInfo> {
    let bad = |m: &str| Error::audio(path, m.to_owned());
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).map_err(|_| bad("truncated FLAC header"))?;
    let mut info: Option<SampleInfo> = None;
    let (mut root_key, mut root_fine_cents, mut loops) = (None, 0.0, vec![]);
    loop {
        let mut h = [0u8; 4];
        r.read_exact(&mut h).map_err(|_| bad("truncated FLAC metadata"))?;
        let last = h[0] & 0x80 != 0;
        let kind = h[0] & 0x7F;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        match kind {
            0 | 2 => {
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).map_err(|_| bad("truncated FLAC metadata block"))?;
                if kind == 0 {
                    if body.len() < 18 {
                        return Err(bad("STREAMINFO too short"));
                    }
                    let sr = (u32::from(body[10]) << 12) | (u32::from(body[11]) << 4) | (u32::from(body[12]) >> 4);
                    let channels = u16::from((body[12] >> 1) & 0x07) + 1;
                    let bits = (u16::from(body[12] & 0x01) << 4 | u16::from(body[13] >> 4)) + 1;
                    let frames = (u64::from(body[13] & 0x0F) << 32)
                        | u64::from(u32::from_be_bytes([body[14], body[15], body[16], body[17]]));
                    info = Some(SampleInfo {
                        format: AudioFormat::Flac,
                        sample_rate: sr,
                        channels,
                        bits_per_sample: bits,
                        float: false,
                        frames,
                        file_size: 0,
                        root_key: None,
                        root_fine_cents: 0.0,
                        loops: vec![],
                    });
                } else if body.len() > 4 && &body[..4] == b"riff" {
                    // foreign RIFF chunks: look for smpl
                    let mut at = 4;
                    while at + 8 <= body.len() {
                        let size = read_u32(&body, at + 4) as usize;
                        let start = at + 8;
                        let end = (start + size).min(body.len());
                        if &body[at..at + 4] == b"smpl" {
                            (root_key, root_fine_cents, loops) = parse_smpl(&body[start..end]);
                        }
                        at = start + size + (size & 1);
                    }
                }
            }
            _ => {
                std::io::copy(&mut r.by_ref().take(len as u64), &mut std::io::sink())
                    .map_err(|_| bad("truncated FLAC metadata block"))?;
            }
        }
        if last {
            break;
        }
    }
    let mut info = info.ok_or_else(|| bad("FLAC file without STREAMINFO"))?;
    info.root_key = root_key;
    info.root_fine_cents = root_fine_cents;
    info.loops = loops;
    Ok(info)
}

fn decode_flac(path: &Path, max_frames: u64) -> Result<Audio> {
    let mut reader = claxon::FlacReader::open(path).map_err(|e| Error::audio(path, e.to_string()))?;
    let si = reader.streaminfo();
    let channels = si.channels as u16;
    let scale = 1.0 / f64::from(1u32 << (si.bits_per_sample.clamp(1, 32) - 1));
    let limit = max_frames.saturating_mul(u64::from(channels));
    let mut samples = Vec::with_capacity(si.samples.map_or(0, |n| (n * u64::from(channels)).min(limit)) as usize);
    for s in reader.samples() {
        if samples.len() as u64 >= limit {
            break;
        }
        let v = s.map_err(|e| Error::audio(path, e.to_string()))?;
        samples.push(f64::from(v) * scale);
    }
    Ok(Audio { sample_rate: si.sample_rate, channels, samples })
}

/// Read many sample headers; failures are kept per file.
pub fn read_infos<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<(PathBuf, Result<SampleInfo>)> {
    paths.into_iter().map(|p| (p.to_path_buf(), SampleInfo::read(p))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small WAV file built in memory.
    pub(crate) fn wav_bytes(rate: u32, channels: u16, bits: u16, float: bool, frames: &[f64], smpl: Option<(u32, &[(u32, u32)])>) -> Vec<u8> {
        let width = usize::from(bits / 8);
        let mut data = vec![];
        for &v in frames {
            for _ in 0..channels {
                match (float, bits) {
                    (true, 32) => data.extend_from_slice(&(v as f32).to_le_bytes()),
                    (false, 16) => data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
                    (false, 24) => data.extend_from_slice(&((v * 8_388_607.0) as i32).to_le_bytes()[..3]),
                    _ => data.extend(std::iter::repeat_n(0u8, width)),
                }
            }
        }
        let block = channels * bits / 8;
        let mut body = b"WAVE".to_vec();
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&(if float { 3u16 } else { 1 }).to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&rate.to_le_bytes());
        body.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        body.extend_from_slice(&block.to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(b"abc\0");
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
        if let Some((root, loops)) = smpl {
            let mut s = vec![];
            for v in [0, 0, 22675, root, 0, 0, 0, loops.len() as u32, 0] {
                s.extend_from_slice(&v.to_le_bytes());
            }
            for &(a, b) in loops {
                for v in [0, 0, a, b, 0, 0] {
                    s.extend_from_slice(&v.to_le_bytes());
                }
            }
            body.extend_from_slice(b"smpl");
            body.extend_from_slice(&(s.len() as u32).to_le_bytes());
            body.extend_from_slice(&s);
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flux-sfz-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn wav_headers_and_loops() {
        let frames: Vec<f64> = (0..1001).map(|i| (i as f64 * 0.01).sin() * 0.5).collect();
        let p = temp("a.wav", &wav_bytes(48000, 2, 24, false, &frames, Some((64, &[(100, 900)]))));
        let i = SampleInfo::read(&p).unwrap();
        assert_eq!((i.sample_rate, i.channels, i.bits_per_sample, i.frames), (48000, 2, 24, 1001));
        assert_eq!(i.encoding(), "PCM_24");
        assert_eq!(i.root_key, Some(64));
        assert_eq!(i.loops, vec![SampleLoop { start: 100, end: 900, kind: 0, play_count: 0 }]);
        let a = Audio::read(&p).unwrap();
        assert_eq!(a.frames(), 1001);
        let m = a.mono();
        assert!((m[500] - frames[500]).abs() < 1e-6);
        let short = Audio::read_frames(&p, 10).unwrap();
        assert_eq!(short.frames(), 10);
    }

    #[test]
    fn float_and_16_bit() {
        let frames = [0.0, 0.5, -0.5, 0.25];
        let p = temp("f.wav", &wav_bytes(44100, 1, 32, true, &frames, None));
        assert_eq!(Audio::read(&p).unwrap().samples, frames.to_vec());
        assert_eq!(SampleInfo::read(&p).unwrap().encoding(), "FLOAT");
        let p = temp("i.wav", &wav_bytes(44100, 1, 16, false, &frames, None));
        let a = Audio::read(&p).unwrap();
        assert!((a.samples[1] - 16383.0 / 32768.0).abs() < 1e-12);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for bytes in [&b""[..], b"RIFF", b"RIFF\x10\0\0\0WAVEfmt \xff\xff\xff\xff", b"fLaC\x00\x00", b"OggS1234"] {
            let p = temp("bad.wav", bytes);
            assert!(SampleInfo::read(&p).is_err());
            assert!(Audio::read(&p).is_err());
        }
        // a frame smaller than its channels (block_align 2 for 2 x 16-bit), and 16-bit float
        let good = wav_bytes(44100, 2, 16, false, &[0.1, 0.2, 0.3], None);
        for (at, value) in [(32usize, 2u16), (20, 3)] {
            let mut bytes = good.clone();
            bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
            let p = temp("bad_layout.wav", &bytes);
            let e = SampleInfo::read(&p).unwrap_err().to_string();
            assert!(e.contains("unsupported WAV layout"), "{e}");
            assert!(Audio::read(&p).is_err());
        }
    }
}
