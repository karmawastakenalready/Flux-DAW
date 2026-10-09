//! Runs a rig's insert chain over a mono WAV file, offline (latency compensated, same length as the input), as
//! `rig.py` does after rendering the source.
//!
//! ```text
//! cargo run -p flux-nam --release --example rig_process -- <rig.json> <root> <in.wav> <out.wav>
//!     [--param name.field=JSON]... [--quality high|scipy] [--ir-resample high|juce] [--block N]
//!     [--submodel INDEX] [--mean]
//! ```
//!
//! The input's first channel is used (or the mean of all channels with `--mean`); the output is mono 32-bit
//! float at the input's rate.

use flux_nam::{ChainOptions, IrResampling, NoHooks, Quality, Rig, RigChain, SubmodelChoice};
use std::time::Instant;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

fn main() {
    let mut pos = vec![];
    let mut params: Vec<(String, serde_json::Value)> = vec![];
    let mut options = ChainOptions::default();
    let mut block = 512usize;
    let mut mean = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| fail(format!("{a} needs a value")));
        match a.as_str() {
            "--param" => {
                let p = val();
                let (k, v) = p.split_once('=').unwrap_or_else(|| fail(format!("--param {p}: expected name.field=JSON")));
                let v = serde_json::from_str(v).unwrap_or_else(|e| fail(format!("--param {p}: {e}")));
                params.push((k.to_owned(), v));
            }
            "--quality" => {
                options.resample = match val().as_str() {
                    "high" => Quality::High,
                    "scipy" => Quality::Scipy,
                    q => fail(format!("--quality: unknown {q:?}")),
                }
            }
            "--ir-resample" => {
                options.ir_resample = match val().as_str() {
                    "high" => IrResampling::High,
                    "juce" => IrResampling::Juce,
                    q => fail(format!("--ir-resample: unknown {q:?}")),
                }
            }
            "--block" => block = val().parse().unwrap_or_else(|e| fail(format!("--block: {e}"))),
            "--submodel" => {
                options.submodel =
                    Some(SubmodelChoice::Index(val().parse().unwrap_or_else(|e| fail(format!("--submodel: {e}")))))
            }
            "--mean" => mean = true,
            s if s.starts_with("--") => fail(format!("unknown option {s}")),
            _ => pos.push(a),
        }
    }
    let [rig_path, root, input, output] = pos.as_slice() else {
        fail(
            "usage: rig_process <rig.json> <root> <in.wav> <out.wav> [--param name.field=JSON]... \
             [--quality high|scipy] [--ir-resample high|juce] [--block N] [--submodel INDEX] [--mean]",
        )
    };

    let rig = Rig::load(rig_path, root).unwrap_or_else(|e| fail(e));
    let rig = rig.with_overrides(params.iter().map(|(k, v)| (k.as_str(), v))).unwrap_or_else(|e| fail(e));

    let mut reader = hound::WavReader::open(input).unwrap_or_else(|e| fail(format!("{input}: {e}")));
    let spec = reader.spec();
    let ch = spec.channels as usize;
    let data: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()
        }
    }
    .unwrap_or_else(|e| fail(format!("{input}: {e}")));
    let x: Vec<f32> = data
        .chunks_exact(ch)
        .map(|f| if mean { f.iter().sum::<f32>() / ch as f32 } else { f[0] })
        .collect();

    let t0 = Instant::now();
    let mut chain =
        RigChain::with_options(&rig, spec.sample_rate, block, &mut NoHooks, options).unwrap_or_else(|e| fail(e));
    let build = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let y = chain.process_offline(&x);
    let run = t0.elapsed().as_secs_f64();
    eprintln!(
        "{} stages at {} Hz, latency {} samples; built in {:.3} s, processed {:.2} s of audio in {:.3} s",
        chain.len(),
        spec.sample_rate,
        chain.latency(),
        build,
        x.len() as f64 / spec.sample_rate as f64,
        run
    );

    let out_spec =
        hound::WavSpec { channels: 1, sample_rate: spec.sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(output, out_spec).unwrap_or_else(|e| fail(format!("{output}: {e}")));
    for v in y {
        w.write_sample(v).unwrap_or_else(|e| fail(format!("{output}: {e}")));
    }
    w.finalize().unwrap_or_else(|e| fail(format!("{output}: {e}")));
}
