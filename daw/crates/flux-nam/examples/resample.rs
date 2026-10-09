//! Resamples a mono 32-bit float WAV offline with the crate's streaming [`Resampler`], zero phase (output sample
//! `j` at input time `j · from / to`, like `scipy.signal.resample_poly`), output length `⌈n · to / from⌉`.
//!
//! ```text
//! cargo run -p flux-nam --release --example resample -- <in.wav> <out.wav> <to_hz> [--quality high|scipy]
//!     [--block N]
//! ```

use flux_nam::Quality;
use flux_nam::resample::Resampler;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

fn main() {
    let mut pos = vec![];
    let mut quality = Quality::High;
    let mut block = 512usize;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| fail(format!("{a} needs a value")));
        match a.as_str() {
            "--quality" => {
                quality = match val().as_str() {
                    "high" => Quality::High,
                    "scipy" => Quality::Scipy,
                    q => fail(format!("--quality: unknown {q:?}")),
                }
            }
            "--block" => block = val().parse().unwrap_or_else(|e| fail(format!("--block: {e}"))),
            s if s.starts_with("--") => fail(format!("unknown option {s}")),
            _ => pos.push(a),
        }
    }
    let [input, output, to] = pos.as_slice() else {
        fail("usage: resample <in.wav> <out.wav> <to_hz> [--quality high|scipy] [--block N]")
    };
    let to: u32 = to.parse().unwrap_or_else(|e| fail(format!("to_hz: {e}")));
    if block == 0 {
        fail("--block must be positive");
    }
    let mut reader = hound::WavReader::open(input).unwrap_or_else(|e| fail(format!("{input}: {e}")));
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Float || spec.channels != 1 {
        fail(format!("{input}: expected a mono 32-bit float WAV"));
    }
    let x: Vec<f32> = reader.samples::<f32>().collect::<Result<_, _>>().unwrap_or_else(|e| fail(format!("{input}: {e}")));
    let mut r = Resampler::new(spec.sample_rate, to, quality, block).unwrap_or_else(|e| fail(e));
    let want = r.max_output(x.len());
    let mut y = Vec::with_capacity(want);
    let mut out = vec![0.0f32; r.max_output(block)];
    // the signal, then silence until every output sample of the signal has been produced
    let zeros = vec![0.0f32; block];
    let mut fed = 0;
    while y.len() < want {
        let part = if fed < x.len() { &x[fed..(fed + block).min(x.len())] } else { &zeros[..] };
        fed += part.len();
        let n = r.process(part, &mut out);
        y.extend_from_slice(&out[..n]);
    }
    y.truncate(want);
    let (up, down) = r.ratio();
    eprintln!("{} Hz -> {to} Hz: {up}/{down}, {} taps per output", spec.sample_rate, r.taps());
    let out_spec = hound::WavSpec { sample_rate: to, ..spec };
    let mut w = hound::WavWriter::create(output, out_spec).unwrap_or_else(|e| fail(format!("{output}: {e}")));
    for v in y {
        w.write_sample(v).unwrap_or_else(|e| fail(format!("{output}: {e}")));
    }
    w.finalize().unwrap_or_else(|e| fail(format!("{output}: {e}")));
}
