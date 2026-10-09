//! Runs a `.nam` model over a mono WAV file. At the model's rate the network runs directly; at another rate the
//! file goes through a [`NamStage`] (resampling both ways), latency compensated so the output lines up with the
//! input. Output: mono 32-bit float WAV of the input's length and rate.
//!
//! ```text
//! cargo run -p flux-nam --release --example nam_run -- <model.nam> <in.wav> <out.wav>
//!     [--submodel INDEX] [--quality high|scipy] [--block N]
//! ```

use flux_nam::{NamModel, NamStage, Quality, SubmodelChoice};

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

fn main() {
    let mut pos = vec![];
    let mut choice = SubmodelChoice::Largest;
    let mut quality = Quality::High;
    let mut block = 512usize;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| fail(format!("{a} needs a value")));
        match a.as_str() {
            "--submodel" => choice = SubmodelChoice::Index(val().parse().unwrap_or_else(|e| fail(format!("--submodel: {e}")))),
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
    let [model_path, input, output] = pos.as_slice() else {
        fail("usage: nam_run <model.nam> <in.wav> <out.wav> [--submodel INDEX] [--quality high|scipy] [--block N]")
    };
    if block == 0 {
        fail("--block must be positive");
    }
    let model = NamModel::load(model_path, choice).unwrap_or_else(|e| fail(e));
    let mut reader = hound::WavReader::open(input).unwrap_or_else(|e| fail(format!("{input}: {e}")));
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Float || spec.channels != 1 {
        fail(format!("{input}: expected a mono 32-bit float WAV"));
    }
    let x: Vec<f32> = reader.samples::<f32>().collect::<Result<_, _>>().unwrap_or_else(|e| fail(format!("{input}: {e}")));

    let y = if spec.sample_rate == model.sample_rate() {
        let mut net = model.instance();
        let mut y = x.clone();
        for part in y.chunks_mut(block) {
            net.process(part);
        }
        y
    } else {
        let mut stage =
            NamStage::new(&model, 0.0, 0.0, spec.sample_rate, block, quality).unwrap_or_else(|e| fail(e));
        let d = stage.latency();
        let mut y = x.clone();
        y.resize(x.len() + d, 0.0);
        for part in y.chunks_mut(block) {
            stage.process(part);
        }
        y.drain(..d);
        eprintln!("resampled {} Hz <-> {} Hz, latency {d} samples", spec.sample_rate, model.sample_rate());
        y
    };
    let mut w = hound::WavWriter::create(output, spec).unwrap_or_else(|e| fail(format!("{output}: {e}")));
    for v in y {
        w.write_sample(v).unwrap_or_else(|e| fail(format!("{output}: {e}")));
    }
    w.finalize().unwrap_or_else(|e| fail(format!("{output}: {e}")));
}
