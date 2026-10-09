//! Measures NAM inference speed: the realtime factor (processing time / audio time) of one instance and of
//! several instances run one after the other on one thread, as a mixer thread would.
//!
//! ```text
//! cargo run -p flux-nam --release --example nam_bench -- <model.nam> [--rate 48000] [--block 512]
//!     [--seconds 10] [--instances 4] [--submodel INDEX] [--quality high|scipy]
//! ```
//!
//! With `--rate` different from the model's rate the measurement includes resampling ([`NamStage`]).

use flux_nam::{NamModel, NamStage, Quality, SubmodelChoice};
use std::time::Instant;

struct Args {
    model: String,
    rate: Option<u32>,
    block: usize,
    seconds: f64,
    instances: usize,
    submodel: SubmodelChoice,
    quality: Quality,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        model: String::new(),
        rate: None,
        block: 512,
        seconds: 10.0,
        instances: 4,
        submodel: SubmodelChoice::Largest,
        quality: Quality::High,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--rate" => a.rate = Some(val("--rate")?.parse().map_err(|e| format!("--rate: {e}"))?),
            "--block" => a.block = val("--block")?.parse().map_err(|e| format!("--block: {e}"))?,
            "--seconds" => a.seconds = val("--seconds")?.parse().map_err(|e| format!("--seconds: {e}"))?,
            "--instances" => a.instances = val("--instances")?.parse().map_err(|e| format!("--instances: {e}"))?,
            "--submodel" => {
                a.submodel = SubmodelChoice::Index(val("--submodel")?.parse().map_err(|e| format!("--submodel: {e}"))?)
            }
            "--quality" => {
                a.quality = match val("--quality")?.as_str() {
                    "high" => Quality::High,
                    "scipy" => Quality::Scipy,
                    q => return Err(format!("--quality: unknown {q:?}")),
                }
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.model = s.to_owned(),
        }
    }
    if a.model.is_empty() {
        return Err("usage: nam_bench <model.nam> [--rate HZ] [--block N] [--seconds S] [--instances N] \
                    [--submodel INDEX] [--quality high|scipy]"
            .into());
    }
    if a.block == 0 || a.instances == 0 || a.seconds <= 0.0 {
        return Err("--block, --instances and --seconds must be positive".into());
    }
    Ok(a)
}

/// A deterministic test signal: noise bursts and decaying tones around guitar levels.
fn signal(n: usize, rate: f64) -> Vec<f32> {
    let mut s = 12345u32;
    (0..n)
        .map(|i| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            let t = i as f64 / rate;
            let env = (-(t % 0.5) * 6.0).exp() as f32;
            let tone = (2.0 * std::f64::consts::PI * 110.0 * t).sin() as f32;
            0.3 * env * (0.7 * tone + 0.3 * noise)
        })
        .collect()
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let model = match NamModel::load(&args.model, args.submodel) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let rate = args.rate.unwrap_or(model.sample_rate());
    let sub = model.submodel().map(|s| format!(", submodel {} (max_value {})", s.index, s.max_value)).unwrap_or_default();
    println!(
        "{}: {} at {} Hz{sub}, receptive field {}; host {} Hz, block {}, {} s, {} build",
        args.model,
        model.architecture(),
        model.sample_rate(),
        model.receptive_field(),
        rate,
        args.block,
        args.seconds,
        if cfg!(debug_assertions) { "debug" } else { "optimised" }
    );
    let n = (args.seconds * rate as f64) as usize;
    let input = signal(n, rate as f64);
    let mut stages: Vec<NamStage> = (0..args.instances)
        .map(|_| NamStage::new(&model, 0.0, 0.0, rate, args.block, args.quality))
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });
    println!("stage latency {} samples", stages[0].latency());
    // warm-up (page in weights, settle the CPU clock)
    let mut warm = input[..n.min(rate as usize)].to_vec();
    for part in warm.chunks_mut(args.block) {
        stages[0].process(part);
    }
    stages[0].reset();

    let mut buf = input.clone();
    let t0 = Instant::now();
    for part in buf.chunks_mut(args.block) {
        stages[0].process(part);
    }
    let one = t0.elapsed().as_secs_f64();
    let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    println!(
        "1 instance: {:.3} s for {:.1} s of audio: realtime factor {:.4} ({:.1}x realtime), output peak {:.3}",
        one,
        args.seconds,
        one / args.seconds,
        args.seconds / one,
        peak
    );

    for s in &mut stages {
        s.reset();
    }
    let mut bufs: Vec<Vec<f32>> = vec![input.clone(); args.instances];
    let mut block_max = 0.0f64;
    let t0 = Instant::now();
    let mut pos = 0;
    while pos < n {
        let len = args.block.min(n - pos);
        let tb = Instant::now();
        for (s, b) in stages.iter_mut().zip(&mut bufs) {
            s.process(&mut b[pos..pos + len]);
        }
        block_max = block_max.max(tb.elapsed().as_secs_f64() / (len as f64 / rate as f64));
        pos += len;
    }
    let all = t0.elapsed().as_secs_f64();
    println!(
        "{} instances: {:.3} s: realtime factor {:.4} total, {:.4} per instance; worst block {:.3} of its duration",
        args.instances,
        all,
        all / args.seconds,
        all / args.seconds / args.instances as f64,
        block_max
    );
}
