//! Pure-Rust FLAC export (replaces `daw/bridge/to_flac.py`).
//!
//! * 16- or 24-bit output from `f32` audio, quantised exactly like the engine's `wav16` export
//!   ([`Dither::Tpdf`] with seed 1 gives the same samples as `flux_engine::dsp::to_i16_tpdf(buf, 1)`), or
//!   lossless from integer PCM / WAV files.
//! * STREAMINFO carries the MD5 of the samples; a SEEKTABLE (one point every 10 s by default) and a
//!   VORBIS_COMMENT block (vendor string, optional `TITLE`/`ARTIST`/any tag, `LOOPSTART`/`LOOPLENGTH`) follow.
//! * By default every encode is decoded again (claxon) and its MD5 compared with the input's, so a file that
//!   is written is known to be lossless.
//!
//! Encoding uses [`flacenc`] (fixed block size, multithreaded).

use anyhow::{Context, Result, anyhow, bail, ensure};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, MetadataBlockData, Stream};
use flacenc::error::{SourceError, Verify};
use flacenc::source::{Fill, MemSource, Source};
use md5::{Digest, Md5};
use serde::Serialize;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;

/// Output bit depth for float input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum BitDepth {
    #[default]
    B16,
    B24,
}

impl BitDepth {
    pub fn bits(self) -> u32 {
        match self {
            BitDepth::B16 => 16,
            BitDepth::B24 => 24,
        }
    }

    /// Full-scale multiplier and clamp range.
    fn range(self) -> (f64, f64, f64) {
        match self {
            BitDepth::B16 => (32767.0, -32768.0, 32767.0),
            BitDepth::B24 => (8_388_607.0, -8_388_608.0, 8_388_607.0),
        }
    }
}

/// How float samples are turned into integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Dither {
    /// Round to nearest (ties away from zero).
    None,
    /// Triangular dither of ±1 LSB from a xorshift64 generator seeded with `seed`, the same as the engine.
    Tpdf { seed: u64 },
}

impl Default for Dither {
    /// TPDF with seed 1, which is what `wav16` export uses.
    fn default() -> Self {
        Dither::Tpdf { seed: 1 }
    }
}

/// Loop region written as the `LOOPSTART` / `LOOPLENGTH` tags (in samples per channel).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LoopPoints {
    pub start: u64,
    pub length: u64,
}

/// Encoder settings. `FlacOptions::default()` matches `wav16` export: 16-bit, TPDF seed 1.
#[derive(Clone, Debug, PartialEq)]
pub struct FlacOptions {
    /// Bit depth for float input (integer input keeps its own depth).
    pub bits: BitDepth,
    /// Quantisation for float input.
    pub dither: Dither,
    /// Vorbis comments, e.g. `("TITLE", "Capital City")`. Field names are ASCII 0x20-0x7D without `=`.
    pub tags: Vec<(String, String)>,
    /// Adds `LOOPSTART` and `LOOPLENGTH`.
    pub loop_points: Option<LoopPoints>,
    /// Samples per frame (32..=32767; 4096 like libFLAC at 44.1/48 kHz). Sample rates up to 96 kHz are supported.
    pub block_size: usize,
    /// Seek point spacing in seconds; 0 writes no SEEKTABLE.
    pub seek_interval_s: u32,
    /// Encoder threads; `None` uses every core.
    pub threads: Option<usize>,
    /// Decode the result and compare its MD5 with the input's (recommended; adds a few % of encode time).
    pub verify: bool,
}

impl Default for FlacOptions {
    fn default() -> Self {
        Self {
            bits: BitDepth::B16,
            dither: Dither::default(),
            tags: Vec::new(),
            loop_points: None,
            block_size: 4096,
            seek_interval_s: 10,
            threads: None,
            verify: true,
        }
    }
}

impl FlacOptions {
    /// Add a Vorbis comment.
    pub fn tag(mut self, name: &str, value: &str) -> Self {
        self.tags.push((name.to_string(), value.to_string()));
        self
    }

    /// Set the loop region.
    pub fn looped(mut self, start: u64, length: u64) -> Self {
        self.loop_points = Some(LoopPoints { start, length });
        self
    }
}

/// What was written.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FlacReport {
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
    /// Samples per channel.
    pub frames: u64,
    /// Size of the FLAC file.
    pub bytes: u64,
    /// STREAMINFO MD5 of the samples, hex.
    pub md5: String,
    /// Float samples that were outside full scale and got clamped.
    pub clipped: u64,
    /// NaN or infinite float samples (written as 0 or full scale, like the engine does).
    pub non_finite: u64,
    /// Whether the output was decoded and matched.
    pub verified: bool,
    pub encode_seconds: f64,
    pub verify_seconds: f64,
}

impl FlacReport {
    pub fn duration_seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate.max(1))
    }

    /// FLAC size relative to the same samples as PCM.
    pub fn ratio(&self) -> f64 {
        let pcm = self.frames * u64::from(self.channels) * u64::from(self.bits.div_ceil(8));
        if pcm == 0 { 0.0 } else { self.bytes as f64 / pcm as f64 }
    }
}

/// Turns floats into integers; keeps clip counts. Identical arithmetic to `flux_engine::dsp::to_i16_tpdf`.
#[derive(Clone, Debug)]
pub struct Quantizer {
    scale: f64,
    lo: f64,
    hi: f64,
    rng: Option<u64>,
    pub clipped: u64,
    pub non_finite: u64,
}

impl Quantizer {
    pub fn new(bits: BitDepth, dither: Dither) -> Self {
        let (scale, lo, hi) = bits.range();
        let rng = match dither {
            Dither::None => None,
            Dither::Tpdf { seed } => Some(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
        };
        Self { scale, lo, hi, rng, clipped: 0, non_finite: 0 }
    }

    fn uniform(s: &mut u64) -> f64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        (*s >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Next sample (call in interleaved order: frame by frame, channel by channel).
    #[inline]
    pub fn quantize(&mut self, x: f32) -> i32 {
        let d = match &mut self.rng {
            Some(s) => {
                let a = Self::uniform(s);
                let b = Self::uniform(s);
                a - b
            }
            None => 0.0,
        };
        let v = (x as f64 * self.scale + d).round();
        if !x.is_finite() {
            self.non_finite += 1;
        } else if v < self.lo || v > self.hi {
            self.clipped += 1;
        }
        // NaN stays NaN through clamp and casts to 0, as in the engine.
        v.clamp(self.lo, self.hi) as i32
    }
}

/// Quantise planar float channels into interleaved integers (the samples the FLAC will contain).
pub fn quantize_interleaved(channels: &[&[f32]], bits: BitDepth, dither: Dither) -> Result<(Vec<i32>, Quantizer)> {
    let frames = check_planar(channels)?;
    let mut q = Quantizer::new(bits, dither);
    let mut out = Vec::with_capacity(frames * channels.len());
    for i in 0..frames {
        for c in channels {
            out.push(q.quantize(c[i]));
        }
    }
    Ok((out, q))
}

fn check_planar(channels: &[&[f32]]) -> Result<usize> {
    ensure!(!channels.is_empty() && channels.len() <= 8, "FLAC needs 1 to 8 channels, got {}", channels.len());
    let frames = channels[0].len();
    if let Some(c) = channels.iter().position(|c| c.len() != frames) {
        bail!("channel {} has {} samples but channel 1 has {frames}", c + 1, channels[c].len());
    }
    Ok(frames)
}

/// Planar float input, quantised block by block (so no full-length integer copy is kept).
struct PlanarF32<'a> {
    chans: &'a [&'a [f32]],
    frames: usize,
    pos: usize,
    bits: usize,
    rate: usize,
    q: Quantizer,
    buf: Vec<i32>,
}

impl Source for PlanarF32<'_> {
    fn channels(&self) -> usize {
        self.chans.len()
    }
    fn bits_per_sample(&self) -> usize {
        self.bits
    }
    fn sample_rate(&self) -> usize {
        self.rate
    }
    fn read_samples<F: Fill>(&mut self, block_size: usize, dest: &mut F) -> Result<usize, SourceError> {
        let n = block_size.min(self.frames - self.pos);
        self.buf.clear();
        for i in self.pos..self.pos + n {
            for c in self.chans {
                self.buf.push(self.q.quantize(c[i]));
            }
        }
        dest.fill_interleaved(&self.buf)?;
        self.pos += n;
        Ok(n)
    }
    fn len_hint(&self) -> Option<usize> {
        Some(self.frames)
    }
}

/// Interleaved integer input.
struct Interleaved<'a, T> {
    data: &'a [T],
    channels: usize,
    pos: usize,
    bits: usize,
    rate: usize,
    buf: Vec<i32>,
}

impl<T: Copy + Into<i32>> Source for Interleaved<'_, T> {
    fn channels(&self) -> usize {
        self.channels
    }
    fn bits_per_sample(&self) -> usize {
        self.bits
    }
    fn sample_rate(&self) -> usize {
        self.rate
    }
    fn read_samples<F: Fill>(&mut self, block_size: usize, dest: &mut F) -> Result<usize, SourceError> {
        let frames = self.data.len() / self.channels;
        let n = block_size.min(frames - self.pos);
        self.buf.clear();
        self.buf.extend(self.data[self.pos * self.channels..(self.pos + n) * self.channels].iter().map(|&s| s.into()));
        dest.fill_interleaved(&self.buf)?;
        self.pos += n;
        Ok(n)
    }
    fn len_hint(&self) -> Option<usize> {
        Some(self.data.len() / self.channels)
    }
}

/// Encode planar stereo float audio (the engine's `Stereo` buffers). Returns the file bytes.
pub fn encode_f32_stereo(sample_rate: u32, left: &[f32], right: &[f32], opts: &FlacOptions) -> Result<(Vec<u8>, FlacReport)> {
    encode_f32(sample_rate, &[left, right], opts)
}

/// Encode planar float audio with 1 to 8 channels.
pub fn encode_f32(sample_rate: u32, channels: &[&[f32]], opts: &FlacOptions) -> Result<(Vec<u8>, FlacReport)> {
    let frames = check_planar(channels)?;
    let bits = opts.bits.bits() as usize;
    let mut src = PlanarF32 {
        chans: channels,
        frames,
        pos: 0,
        bits,
        rate: sample_rate as usize,
        q: Quantizer::new(opts.bits, opts.dither),
        buf: Vec::new(),
    };
    let (bytes, mut report) = encode_source(&mut src, frames, opts)?;
    report.clipped = src.q.clipped;
    report.non_finite = src.q.non_finite;
    Ok((bytes, report))
}

/// Encode interleaved integer PCM losslessly. Samples must fit in `bits` (8, 16 or 24).
pub fn encode_pcm(sample_rate: u32, channels: u32, bits: u32, interleaved: &[i32], opts: &FlacOptions) -> Result<(Vec<u8>, FlacReport)> {
    ensure!(matches!(bits, 8 | 16 | 24), "FLAC export supports 8, 16 or 24-bit integer samples, got {bits}");
    let ch = check_interleaved(channels, interleaved.len())?;
    let (lo, hi) = (-(1i32 << (bits - 1)), (1i32 << (bits - 1)) - 1);
    if let Some(i) = interleaved.iter().position(|&s| s < lo || s > hi) {
        bail!("sample {i} ({}) does not fit in {bits} bits", interleaved[i]);
    }
    let mut src = Interleaved { data: interleaved, channels: ch, pos: 0, bits: bits as usize, rate: sample_rate as usize, buf: Vec::new() };
    encode_source(&mut src, interleaved.len() / ch, opts)
}

/// Encode interleaved 16-bit PCM (e.g. the engine's `to_i16_tpdf` output) losslessly.
pub fn encode_i16(sample_rate: u32, channels: u32, interleaved: &[i16], opts: &FlacOptions) -> Result<(Vec<u8>, FlacReport)> {
    let ch = check_interleaved(channels, interleaved.len())?;
    let mut src = Interleaved { data: interleaved, channels: ch, pos: 0, bits: 16, rate: sample_rate as usize, buf: Vec::new() };
    encode_source(&mut src, interleaved.len() / ch, opts)
}

fn check_interleaved(channels: u32, len: usize) -> Result<usize> {
    ensure!((1..=8).contains(&channels), "FLAC needs 1 to 8 channels, got {channels}");
    let ch = channels as usize;
    ensure!(len.is_multiple_of(ch), "{len} samples cannot be split into {ch} channels");
    Ok(ch)
}

/// Write planar stereo float audio to a FLAC file (atomically: a failed export leaves no partial file).
pub fn write_f32_stereo(path: &Path, sample_rate: u32, left: &[f32], right: &[f32], opts: &FlacOptions) -> Result<FlacReport> {
    let (bytes, report) = encode_f32_stereo(sample_rate, left, right, opts)?;
    write_file(path, &bytes)?;
    Ok(report)
}

/// Write interleaved 16-bit PCM to a FLAC file.
pub fn write_i16(path: &Path, sample_rate: u32, channels: u32, interleaved: &[i16], opts: &FlacOptions) -> Result<FlacReport> {
    let (bytes, report) = encode_i16(sample_rate, channels, interleaved, opts)?;
    write_file(path, &bytes)?;
    Ok(report)
}

/// Convert a WAV file. Integer WAVs (8/16/24-bit) are copied losslessly at their own depth; 32-bit float WAVs
/// are quantised with `opts.bits` and `opts.dither`.
pub fn wav_to_flac(src: &Path, dst: &Path, opts: &FlacOptions) -> Result<FlacReport> {
    let mut r = hound::WavReader::open(src).with_context(|| format!("opening {}", src.display()))?;
    let spec = r.spec();
    let ctx = || format!("reading {}", src.display());
    let (bytes, report) = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => {
            let s: Vec<i16> = r.samples::<i16>().collect::<Result<_, _>>().with_context(ctx)?;
            encode_i16(spec.sample_rate, u32::from(spec.channels), &s, opts)?
        }
        (hound::SampleFormat::Int, b @ (8 | 24)) => {
            let s: Vec<i32> = r.samples::<i32>().collect::<Result<_, _>>().with_context(ctx)?;
            encode_pcm(spec.sample_rate, u32::from(spec.channels), u32::from(b), &s, opts)?
        }
        (hound::SampleFormat::Float, 32) => {
            let s: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().with_context(ctx)?;
            let ch = check_interleaved(u32::from(spec.channels), s.len())?;
            let planar: Vec<Vec<f32>> = (0..ch).map(|c| s.iter().skip(c).step_by(ch).copied().collect()).collect();
            let refs: Vec<&[f32]> = planar.iter().map(Vec::as_slice).collect();
            encode_f32(spec.sample_rate, &refs, opts)?
        }
        (f, b) => bail!("{}: {b}-bit {f:?} WAV is not supported (use 8/16/24-bit integer or 32-bit float)", src.display()),
    };
    write_file(dst, &bytes)?;
    Ok(report)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    crate::fsutil::atomic_write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn encoder_config(opts: &FlacOptions, frames: usize, threads: Option<usize>) -> Result<flacenc::error::Verified<flacenc::config::Encoder>> {
    let mut cfg = flacenc::config::Encoder::default();
    cfg.block_size = opts.block_size;
    let threads = threads.or(opts.threads);
    // Threads only pay off once there are a few frames per worker.
    cfg.multithread = threads != Some(1) && frames >= opts.block_size * 8;
    cfg.workers = threads.and_then(NonZeroUsize::new);
    cfg.into_verified().map_err(|(_, e)| anyhow!("invalid FLAC encoder settings: {e}"))
}

fn encode_source<S: Source>(src: &mut S, frames: usize, opts: &FlacOptions) -> Result<(Vec<u8>, FlacReport)> {
    let (rate, channels, bits) = (src.sample_rate(), src.channels(), src.bits_per_sample());
    ensure!(frames > 0, "nothing to encode: the audio has no samples");
    ensure!((frames as u64) < (1u64 << 36), "{frames} samples is more than a FLAC stream can describe");
    ensure!(
        (flacenc::constant::MIN_BLOCK_SIZE..=flacenc::constant::MAX_BLOCK_SIZE).contains(&opts.block_size),
        "block size {} is outside {}..={}",
        opts.block_size,
        flacenc::constant::MIN_BLOCK_SIZE,
        flacenc::constant::MAX_BLOCK_SIZE
    );
    ensure!((1..=655_350).contains(&rate), "sample rate {rate} Hz cannot be stored in FLAC");
    if let Some(lp) = opts.loop_points {
        ensure!(lp.length > 0, "loop length must be positive");
        ensure!(
            lp.start.checked_add(lp.length).is_some_and(|end| end <= frames as u64),
            "loop {}+{} runs past the end ({frames} samples)",
            lp.start,
            lp.length
        );
    }
    let comment = vorbis_comment(opts)?;
    // Preflight on one sample with the single-threaded encoder: it reports unsupported parameters as errors,
    // where the multithreaded one would panic in a worker.
    flacenc::encode_with_fixed_block_size(
        &encoder_config(opts, 1, Some(1))?,
        MemSource::from_samples(&vec![0; channels], channels, bits, rate),
        opts.block_size,
    )
    .map_err(|e| anyhow!("cannot encode {channels} channel(s) of {bits}-bit audio at {rate} Hz: {e}"))?;

    let t0 = Instant::now();
    let mut stream = flacenc::encode_with_fixed_block_size(&encoder_config(opts, frames, None)?, &mut *src, opts.block_size)
        .map_err(|e| anyhow!("FLAC encoding failed: {e}"))?;
    ensure!(
        stream.stream_info().total_samples() == frames,
        "encoder consumed {} samples, expected {frames}",
        stream.stream_info().total_samples()
    );
    // A fixed-size stream: libFLAC writes min = max = block size even when the last block is shorter.
    stream
        .stream_info_mut()
        .set_block_sizes(opts.block_size, opts.block_size)
        .map_err(|e| anyhow!("setting block sizes: {e}"))?;
    if opts.seek_interval_s > 0 {
        let table = seek_table(&stream, opts.block_size, rate as u64 * u64::from(opts.seek_interval_s));
        stream.add_metadata_block(MetadataBlockData::new_unknown(3, &table).map_err(|e| anyhow!("seek table: {e}"))?);
    }
    stream.add_metadata_block(MetadataBlockData::new_unknown(4, &comment).map_err(|e| anyhow!("vorbis comment: {e}"))?);
    let mut sink = ByteSink::with_capacity(stream.count_bits());
    stream.write(&mut sink).map_err(|e| anyhow!("writing the FLAC stream: {e}"))?;
    let bytes = sink.into_inner();
    let encode_seconds = t0.elapsed().as_secs_f64();
    let md5 = *stream.stream_info().md5_digest();
    let mut report = FlacReport {
        sample_rate: rate as u32,
        channels: channels as u32,
        bits: bits as u32,
        frames: frames as u64,
        bytes: bytes.len() as u64,
        md5: hex::encode(md5),
        clipped: 0,
        non_finite: 0,
        verified: false,
        encode_seconds,
        verify_seconds: 0.0,
    };
    if opts.verify {
        let t1 = Instant::now();
        let info = decode_check(&bytes).context("the encoded FLAC did not decode back to the input")?;
        ensure!(info.md5 == md5, "decoded samples do not match the input (MD5 {} vs {})", hex::encode(info.md5), report.md5);
        ensure!(
            info.sample_rate == report.sample_rate && info.channels == report.channels && info.bits == report.bits && info.frames == report.frames,
            "decoded stream format differs from the input"
        );
        report.verified = true;
        report.verify_seconds = t1.elapsed().as_secs_f64();
    }
    Ok((bytes, report))
}

/// SEEKTABLE body: a point at the frame holding each multiple of `every` samples.
fn seek_table(stream: &Stream, block_size: usize, every: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    let mut next = 0u64;
    for n in 0..stream.frame_count() {
        let Some(f) = stream.frame(n) else { break };
        let first = n as u64 * block_size as u64;
        let len = f.block_size() as u64;
        if every > 0 && next < first + len {
            out.extend_from_slice(&first.to_be_bytes());
            out.extend_from_slice(&offset.to_be_bytes());
            out.extend_from_slice(&(len as u16).to_be_bytes());
            while next < first + len {
                next += every;
            }
        }
        offset += (f.count_bits() / 8) as u64;
    }
    out
}

/// VORBIS_COMMENT body.
fn vorbis_comment(opts: &FlacOptions) -> Result<Vec<u8>> {
    let mut fields: Vec<(String, String)> = opts.tags.clone();
    if let Some(lp) = opts.loop_points {
        fields.retain(|(k, _)| !k.eq_ignore_ascii_case("LOOPSTART") && !k.eq_ignore_ascii_case("LOOPLENGTH"));
        fields.push(("LOOPSTART".into(), lp.start.to_string()));
        fields.push(("LOOPLENGTH".into(), lp.length.to_string()));
    }
    let vendor = format!("flux-assets {} (flacenc 0.5)", env!("CARGO_PKG_VERSION"));
    let mut out = Vec::new();
    let push_str = |out: &mut Vec<u8>, s: &[u8]| -> Result<()> {
        let n = u32::try_from(s.len()).map_err(|_| anyhow!("tag is too long"))?;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(s);
        Ok(())
    };
    push_str(&mut out, vendor.as_bytes())?;
    out.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    for (k, v) in &fields {
        ensure!(
            !k.is_empty() && k.bytes().all(|b| (0x20..=0x7D).contains(&b) && b != b'='),
            "tag name {k:?} must be printable ASCII without '='"
        );
        push_str(&mut out, format!("{k}={v}").as_bytes())?;
    }
    // Metadata blocks have a 24-bit length.
    ensure!(out.len() < (1 << 24), "tags are too large ({} bytes)", out.len());
    Ok(out)
}

/// What [`decode_check`] found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DecodedInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
    pub frames: u64,
    /// MD5 of the decoded samples (not the one stored in STREAMINFO).
    #[serde(with = "hex_md5")]
    pub md5: [u8; 16],
    /// The MD5 stored in STREAMINFO.
    #[serde(with = "hex_md5")]
    pub stored_md5: [u8; 16],
    pub tags: Vec<(String, String)>,
}

mod hex_md5 {
    pub fn serialize<S: serde::Serializer>(v: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }
}

/// Decode a whole FLAC file (frame CRCs are checked) and hash the samples. Errors when the decoded MD5 differs
/// from STREAMINFO's or the sample count from the header's.
pub fn decode_check(bytes: &[u8]) -> Result<DecodedInfo> {
    let mut r = claxon::FlacReader::new(std::io::Cursor::new(bytes)).map_err(|e| anyhow!("not a valid FLAC stream: {e}"))?;
    let si = r.streaminfo();
    let tags: Vec<(String, String)> = r.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let bps = si.bits_per_sample.div_ceil(8) as usize;
    let mut h = Md5::new();
    let mut frames = 0u64;
    let mut blocks = r.blocks();
    let mut buf = Vec::new();
    let mut bytes_buf = Vec::new();
    while let Some(block) = blocks.read_next_or_eof(buf).map_err(|e| anyhow!("decoding failed after {frames} samples: {e}"))? {
        bytes_buf.clear();
        for i in 0..block.duration() {
            for c in 0..si.channels {
                bytes_buf.extend_from_slice(&block.sample(c, i).to_le_bytes()[..bps]);
            }
        }
        h.update(&bytes_buf);
        frames += u64::from(block.duration());
        buf = block.into_buffer();
    }
    let md5: [u8; 16] = h.finalize().into();
    if let Some(total) = si.samples {
        ensure!(total == frames, "STREAMINFO says {total} samples but {frames} were decoded");
    }
    ensure!(si.md5sum == [0; 16] || si.md5sum == md5, "decoded MD5 {} differs from STREAMINFO's {}", hex::encode(md5), hex::encode(si.md5sum));
    Ok(DecodedInfo { sample_rate: si.sample_rate, channels: si.channels, bits: si.bits_per_sample, frames, md5, stored_md5: si.md5sum, tags })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8]) -> (claxon::metadata::StreamInfo, Vec<i32>) {
        let mut r = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
        let si = r.streaminfo();
        let s: Vec<i32> = r.samples().map(|x| x.unwrap()).collect();
        (si, s)
    }

    fn sine(n: usize, f: f32, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * f * std::f32::consts::TAU / 48_000.0).sin()).collect()
    }

    #[test]
    fn float_round_trip_16_and_24() {
        let l = sine(48_000 + 17, 440.0, 0.5);
        let r = sine(48_000 + 17, 660.0, 0.4);
        for bits in [BitDepth::B16, BitDepth::B24] {
            for dither in [Dither::None, Dither::Tpdf { seed: 1 }, Dither::Tpdf { seed: 99 }] {
                let opts = FlacOptions { bits, dither, ..Default::default() };
                let (bytes, rep) = encode_f32_stereo(48_000, &l, &r, &opts).unwrap();
                assert!(rep.verified);
                let (want, _) = quantize_interleaved(&[&l, &r], bits, dither).unwrap();
                let (si, got) = decode_all(&bytes);
                assert_eq!(si.bits_per_sample, bits.bits());
                assert_eq!(si.samples, Some(l.len() as u64));
                assert_eq!(si.min_block_size, 4096);
                assert_eq!(si.max_block_size, 4096);
                assert_eq!(got, want, "{bits:?} {dither:?}");
                // Independent MD5 of the expected samples.
                let mut h = Md5::new();
                let bps = bits.bits().div_ceil(8) as usize;
                for s in &want {
                    h.update(&s.to_le_bytes()[..bps]);
                }
                let md5: [u8; 16] = h.finalize().into();
                assert_eq!(si.md5sum, md5);
                assert_eq!(rep.md5, hex::encode(md5));
            }
        }
    }

    #[test]
    fn quantizer_edges() {
        let mut q = Quantizer::new(BitDepth::B16, Dither::None);
        assert_eq!(q.quantize(1.0), 32767);
        assert_eq!(q.quantize(-1.0), -32767);
        assert_eq!(q.quantize(2.0), 32767);
        assert_eq!(q.quantize(-2.0), -32768);
        assert_eq!(q.quantize(f32::NAN), 0);
        assert_eq!(q.quantize(f32::INFINITY), 32767);
        assert_eq!(q.quantize(f32::NEG_INFINITY), -32768);
        assert_eq!((q.clipped, q.non_finite), (2, 3));
        assert_eq!(q.quantize(0.25 / 32767.0), 0);
        assert_eq!(q.quantize(0.75 / 32767.0), 1);
        let mut q = Quantizer::new(BitDepth::B24, Dither::None);
        assert_eq!(q.quantize(1.0), 8_388_607);
        assert_eq!(q.quantize(-1.5), -8_388_608);
    }

    /// Values from an independent Python implementation of the engine's TPDF quantiser, so the arithmetic stays
    /// pinned even without the `flux-engine` comparison in `tests/flac_engine.rs`.
    #[test]
    fn tpdf_matches_reference_values() {
        let x: [f32; 16] =
            [0.0, 0.0, 0.5, -0.5, 0.25, -0.25, 1e-4, -1e-4, 0.75, 0.125, 1.5, -1.5, 1.0 / 32768.0, -1.0 / 32768.0, 0.9999, -0.9999];
        let left: Vec<f32> = x.iter().step_by(2).copied().collect();
        let right: Vec<f32> = x.iter().skip(1).step_by(2).copied().collect();
        let cases: [(BitDepth, u64, [i32; 16]); 4] = [
            (BitDepth::B16, 1, [0, 0, 16383, -16384, 8192, -8192, 4, -3, 24575, 4096, 32767, -32768, 1, -1, 32764, -32764]),
            (BitDepth::B16, 7, [0, 0, 16384, -16384, 8192, -8192, 3, -3, 24575, 4095, 32767, -32768, 1, -1, 32764, -32764]),
            (
                BitDepth::B24,
                1,
                [
                    0, 0, 4194303, -4194304, 2097152, -2097152, 839, -839, 6291455, 1048576, 8388607, -8388608, 256, -256, 8387768,
                    -8387768,
                ],
            ),
            (
                BitDepth::B24,
                7,
                [
                    0, 0, 4194304, -4194304, 2097152, -2097152, 839, -839, 6291455, 1048575, 8388607, -8388608, 256, -256, 8387768,
                    -8387768,
                ],
            ),
        ];
        for (bits, seed, want) in cases {
            let (got, q) = quantize_interleaved(&[&left, &right], bits, Dither::Tpdf { seed }).unwrap();
            assert_eq!(got, want, "{bits:?} seed {seed}");
            assert_eq!(q.clipped, 2);
        }
    }

    #[test]
    fn edge_lengths_and_levels() {
        let opts = FlacOptions::default();
        for n in [1usize, 2, 31, 32, 4095, 4096, 4097, 3 * 4096 + 1] {
            let silence = vec![0.0f32; n];
            let (b, rep) = encode_f32_stereo(44_100, &silence, &silence, &FlacOptions { dither: Dither::None, ..opts.clone() }).unwrap();
            assert_eq!(decode_all(&b).1, vec![0; 2 * n]);
            assert!(rep.verified && rep.frames == n as u64);
            let full: Vec<f32> = (0..n).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
            let (b, _) = encode_f32_stereo(44_100, &full, &full, &opts).unwrap();
            let (want, _) = quantize_interleaved(&[&full, &full], BitDepth::B16, opts.dither).unwrap();
            assert_eq!(decode_all(&b).1, want);
        }
        let n = 70_001;
        let lo = vec![-1.0f32; n];
        let hi = vec![1.0f32; n];
        let (b, rep) = encode_f32_stereo(48_000, &lo, &hi, &FlacOptions { bits: BitDepth::B24, ..opts.clone() }).unwrap();
        let (want, _) = quantize_interleaved(&[&lo, &hi], BitDepth::B24, opts.dither).unwrap();
        assert_eq!(decode_all(&b).1, want);
        assert!(rep.clipped > 0);
    }

    #[test]
    fn integer_input_is_lossless() {
        let pcm: Vec<i16> = (0..20_000).map(|i| ((i * 7919) % 65_536 - 32_768) as i16).collect();
        let (b, rep) = encode_i16(22_050, 2, &pcm, &FlacOptions::default()).unwrap();
        assert!(rep.verified);
        assert_eq!(decode_all(&b).1, pcm.iter().map(|&s| i32::from(s)).collect::<Vec<_>>());
        let pcm24: Vec<i32> = (0..9_000).map(|i| (i * 104_729) % 16_777_216 - 8_388_608).collect();
        let (b, _) = encode_pcm(96_000, 3, 24, &pcm24, &FlacOptions::default()).unwrap();
        assert_eq!(decode_all(&b).1, pcm24);
        assert!(encode_pcm(48_000, 2, 16, &[0, 40_000], &FlacOptions::default()).is_err());
        assert!(encode_pcm(48_000, 2, 16, &[0, 1, 2], &FlacOptions::default()).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        let o = FlacOptions::default();
        assert!(encode_f32_stereo(48_000, &[], &[], &o).is_err());
        assert!(encode_f32_stereo(48_000, &[0.0; 3], &[0.0; 2], &o).is_err());
        assert!(encode_f32_stereo(0, &[0.0; 3], &[0.0; 3], &o).is_err());
        assert!(encode_f32_stereo(700_000, &[0.0; 3], &[0.0; 3], &o).is_err());
        assert!(encode_f32_stereo(48_000, &[0.0; 3], &[0.0; 3], &FlacOptions { block_size: 8, ..o.clone() }).is_err());
        assert!(encode_f32_stereo(48_000, &[0.0; 3], &[0.0; 3], &o.clone().looped(2, 2)).is_err());
        assert!(encode_f32_stereo(48_000, &[0.0; 3], &[0.0; 3], &o.clone().looped(0, 0)).is_err());
        assert!(encode_f32_stereo(48_000, &[0.0; 3], &[0.0; 3], &o.clone().tag("A=B", "x")).is_err());
        let nine: Vec<&[f32]> = vec![&[0.0]; 9];
        assert!(encode_f32(48_000, &nine, &o).is_err());
        // Odd sample rates that FLAC frame headers can still describe.
        for rate in [8_000, 11_025, 37_800, 44_100, 65_535, 96_000] {
            encode_f32_stereo(rate, &[0.1; 100], &[0.2; 100], &o).unwrap_or_else(|e| panic!("{rate}: {e:#}"));
        }
        // flacenc 0.5 stops at 96 kHz; the preflight turns that into an error instead of a worker panic.
        let e = encode_f32_stereo(192_000, &[0.1; 100_000], &[0.2; 100_000], &o).unwrap_err();
        assert!(format!("{e:#}").contains("192000 Hz"), "{e:#}");
        assert!(decode_check(b"fLaC not really").is_err());
    }

    #[test]
    fn tags_loop_points_and_seek_table() {
        let n = 48_000 * 25 + 5;
        let l = sine(n, 220.0, 0.3);
        let opts = FlacOptions::default().tag("TITLE", "Capital City (loop)").tag("ARTIST", "Flux").looped(48_000, 48_000 * 20);
        let (b, _) = encode_f32_stereo(48_000, &l, &l, &opts).unwrap();
        let info = decode_check(&b).unwrap();
        let get = |k: &str| info.tags.iter().find(|(t, _)| t == k).map(|(_, v)| v.as_str());
        assert_eq!(get("TITLE"), Some("Capital City (loop)"));
        assert_eq!(get("ARTIST"), Some("Flux"));
        assert_eq!(get("LOOPSTART"), Some("48000"));
        assert_eq!(get("LOOPLENGTH"), Some("960000"));
        // Walk the metadata blocks: STREAMINFO, SEEKTABLE, VORBIS_COMMENT (last).
        assert_eq!(&b[..4], b"fLaC");
        let mut p = 4;
        let mut kinds = Vec::new();
        let mut seek = Vec::new();
        loop {
            let (last, kind) = (b[p] & 0x80 != 0, b[p] & 0x7F);
            let len = u32::from_be_bytes([0, b[p + 1], b[p + 2], b[p + 3]]) as usize;
            if kind == 3 {
                seek = b[p + 4..p + 4 + len].to_vec();
            }
            kinds.push(kind);
            p += 4 + len;
            if last {
                break;
            }
        }
        assert_eq!(kinds, vec![0, 3, 4]);
        let points: Vec<(u64, u64, u16)> = seek
            .chunks(18)
            .map(|c| {
                (
                    u64::from_be_bytes(c[..8].try_into().unwrap()),
                    u64::from_be_bytes(c[8..16].try_into().unwrap()),
                    u16::from_be_bytes(c[16..].try_into().unwrap()),
                )
            })
            .collect();
        assert_eq!(points.len(), 3); // 0 s, 10 s, 20 s
        assert_eq!(points[0], (0, 0, 4096));
        for (sample, offset, _) in &points {
            assert_eq!(sample % 4096, 0);
            // Every offset must land on a frame sync code.
            let at = p + *offset as usize;
            assert_eq!((b[at], b[at + 1] & 0xFE), (0xFF, 0xF8), "seek point {sample}");
        }
        assert!(points[1].0 <= 480_000 && points[1].0 + 4096 > 480_000);
    }

    #[test]
    fn single_threaded_matches_multithreaded() {
        let l = sine(200_000, 330.0, 0.7);
        let r = sine(200_000, 331.0, 0.7);
        let (a, _) = encode_f32_stereo(48_000, &l, &r, &FlacOptions { threads: Some(1), ..Default::default() }).unwrap();
        let (b, _) = encode_f32_stereo(48_000, &l, &r, &FlacOptions { threads: Some(4), ..Default::default() }).unwrap();
        assert_eq!(decode_all(&a).1, decode_all(&b).1);
    }
}
