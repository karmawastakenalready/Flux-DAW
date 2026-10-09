//! The FLAC export must contain exactly the samples the engine writes for `wav16`.

use flux_assets::flac::{self, BitDepth, Dither, FlacOptions};

fn decode(bytes: &[u8]) -> Vec<i32> {
    let mut r = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
    r.samples().map(|s| s.unwrap()).collect()
}

fn test_signal(n: usize) -> (Vec<f32>, Vec<f32>) {
    // Music-like levels, quiet passages (where dither matters), overs, and non-finite values.
    let l: Vec<f32> = (0..n)
        .map(|i| match i % 9973 {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => 1.3,
            3 => -1.3,
            _ => 0.6 * (i as f32 * 0.0123).sin() * (1.0 + 0.5 * (i as f32 * 0.00007).cos()) * if i % 20_000 < 4000 { 1e-4 } else { 1.0 },
        })
        .collect();
    let r: Vec<f32> = (0..n).map(|i| 0.4 * (i as f32 * 0.0311).sin() + 1e-5 * (i % 7) as f32).collect();
    (l, r)
}

#[test]
fn matches_engine_wav16_samples() {
    for n in [1usize, 2, 4095, 4097, 250_001] {
        let (l, r) = test_signal(n);
        let engine = flux_engine::dsp::to_i16_tpdf(&(l.clone(), r.clone()), 1);
        let (bytes, rep) = flac::encode_f32_stereo(48_000, &l, &r, &FlacOptions::default()).unwrap();
        assert!(rep.verified);
        let got = decode(&bytes);
        assert_eq!(got.len(), engine.len());
        assert!(got.iter().zip(&engine).all(|(a, b)| *a == i32::from(*b)), "n = {n}: FLAC samples differ from wav16");
        // The same through the i16 path (FLAC of an existing wav16 buffer).
        let (bytes2, _) = flac::encode_i16(48_000, 2, &engine, &FlacOptions::default()).unwrap();
        assert_eq!(decode(&bytes2), got);
    }
}

#[test]
fn other_seeds_match_too() {
    let (l, r) = test_signal(30_000);
    for seed in [0u64, 2, 12345, u64::MAX] {
        let engine = flux_engine::dsp::to_i16_tpdf(&(l.clone(), r.clone()), seed);
        let opts = FlacOptions { dither: Dither::Tpdf { seed }, bits: BitDepth::B16, ..Default::default() };
        let (bytes, _) = flac::encode_f32_stereo(44_100, &l, &r, &opts).unwrap();
        assert_eq!(decode(&bytes), engine.iter().map(|&s| i32::from(s)).collect::<Vec<_>>(), "seed {seed}");
    }
}
