//! The rig chain as the engine uses it: streaming in arbitrary blocks, latency, reset, the repository's rigs.

mod common;

use flux_nam::{ChainOptions, NoHooks, Quality, Rig, RigChain};
use common::*;

fn run_blocks(chain: &mut RigChain, x: &[f32], blocks: &[usize]) -> Vec<f32> {
    let mut y = x.to_vec();
    let (mut i, mut k) = (0, 0);
    while i < y.len() {
        let n = blocks[k % blocks.len()].min(y.len() - i);
        chain.process(&mut y[i..i + n]);
        i += n;
        k += 1;
    }
    y
}

#[test]
fn any_block_pattern_gives_the_same_stream() {
    let dir = temp_dir("blocks");
    for rate in [48000u32, 44100] {
        let ir = write_ir(&dir, rate);
        let full = full_rig_json(&ir);
        let mut without_ir = full.clone();
        without_ir["chain"].as_array_mut().unwrap().retain(|st| st["type"] != "ir");
        let x = guitar(rate as usize, rate as f64);
        for (json, exact) in [(without_ir, true), (full, false)] {
            let rig = Rig::parse(&json, &dir, "full.rig.json").unwrap();
            let mut reference = RigChain::new(&rig, rate, 4096, &mut NoHooks).unwrap();
            let want = run_blocks(&mut reference, &x, &[4096]);
            let peak = want.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak > 1e-2, "silent output");
            for (max_block, blocks) in [(64usize, vec![64usize]), (512, vec![1, 511, 17, 300]), (128, vec![1000])] {
                let mut chain = RigChain::new(&rig, rate, max_block, &mut NoHooks).unwrap();
                assert_eq!(chain.latency(), reference.latency());
                let got = run_blocks(&mut chain, &x, &blocks);
                let what = format!("{rate} Hz, ir {}, max_block {max_block}, blocks {blocks:?}", !exact);
                if exact {
                    assert!(want == got, "{what}: output depends on the block pattern");
                } else {
                    // The FFT convolution's partition size follows max_block and rounds differently (about
                    // -135 dB); the float low shelf after it (JUCE's design, a pole pair close to z = 1) turns
                    // that into a different pattern of its own rounding noise, about -90 dB.
                    let power = |v: &mut dyn Iterator<Item = f32>| v.map(|e| (e as f64).powi(2)).sum::<f64>();
                    let ratio = power(&mut want.iter().zip(&got).map(|(a, b)| a - b)) / power(&mut want.iter().copied());
                    assert!(ratio < 1e-8, "{what}: difference {:.1} dB", 10.0 * ratio.log10());
                }
                // reset: the same input gives the same output again
                chain.reset();
                assert_eq!(run_blocks(&mut chain, &x, &blocks), got, "reset: {what}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn latency_and_offline_alignment() {
    let dir = temp_dir("latency");
    let ir = write_ir(&dir, 44100);
    let rig = Rig::parse(&full_rig_json(&ir), &dir, "full.rig.json").unwrap();
    let high = RigChain::new(&rig, 44100, 256, &mut NoHooks).unwrap();
    let scipy = RigChain::with_options(&rig, 44100, 256, &mut NoHooks, ChainOptions { resample: Quality::Scipy, ..ChainOptions::default() })
        .unwrap();
    assert_eq!(high.latency(), 128);
    assert_eq!(scipy.latency(), 20);
    let at48 = RigChain::new(&rig, 48000, 256, &mut NoHooks).unwrap();
    assert_eq!(at48.latency(), 0);
    let stages: Vec<(String, usize)> = high.stage_latencies().map(|(s, l)| (s.to_owned(), l)).collect();
    assert_eq!(stages.len(), 10);
    assert_eq!(stages[2], ("full.rig.json: chain[2] (nam)".to_owned(), 128));

    // process_offline is the stream shifted by the latency
    let x = guitar(20000, 44100.0);
    let mut a = RigChain::new(&rig, 44100, 256, &mut NoHooks).unwrap();
    let offline = a.process_offline(&x);
    assert_eq!(offline.len(), x.len());
    let mut b = RigChain::new(&rig, 44100, 256, &mut NoHooks).unwrap();
    let mut stream = x.clone();
    stream.resize(x.len() + 128, 0.0);
    b.process(&mut stream);
    assert_eq!(&stream[128..], &offline[..]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overrides_reach_the_processing() {
    let dir = temp_dir("overrides");
    let ir = write_ir(&dir, 48000);
    let rig = Rig::parse(&full_rig_json(&ir), &dir, "full.rig.json").unwrap();
    let x = guitar(9600, 48000.0);
    let base = RigChain::new(&rig, 48000, 512, &mut NoHooks).unwrap().process_offline(&x);
    let louder = serde_json::json!(3.0);
    let rig2 = rig.with_overrides([("gain.db", &louder)]).unwrap();
    let y = RigChain::new(&rig2, 48000, 512, &mut NoHooks).unwrap().process_offline(&x);
    // +6 dB relative to -3 dB, applied last
    let g = (10f64.powf(3.0 / 20.0) as f32) / (10f64.powf(-3.0 / 20.0) as f32);
    for (a, b) in base.iter().zip(&y) {
        assert!((a * g - b).abs() <= 1e-6 * b.abs().max(1e-3), "{a} {b}");
    }
    let e = rig.with_overrides([("amp.input_dB", &louder)]).unwrap_err().to_string();
    assert!(e.contains("input_dB"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The repository's rigs (skipped when the instruments are not present).
#[test]
fn repository_rigs_build_and_run() {
    let root = repo_root();
    let rigs = ["gtx_5150_L", "gtx_5150_R", "gtx_evh_lead"];
    for name in rigs {
        let path = root.join("sfz").join("rigs").join(format!("{name}.rig.json"));
        if !path.exists() {
            eprintln!("skipped: {} not found", path.display());
            return;
        }
        let rig = match Rig::load(&path, &root) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipped {name}: {e}");
                continue;
            }
        };
        for (rate, latency) in [(44100u32, 128usize), (48000, 0)] {
            let mut chain = RigChain::new(&rig, rate, 512, &mut NoHooks).unwrap();
            assert_eq!(chain.latency(), latency, "{name} at {rate}");
            let mut x = guitar(rate as usize / 2, rate as f64);
            chain.process(&mut x);
            assert!(x.iter().all(|v| v.is_finite()) && x.iter().any(|v| v.abs() > 1e-3), "{name} at {rate}");
        }
    }
}
