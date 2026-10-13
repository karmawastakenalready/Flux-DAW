//! The FLAC export must contain exactly the samples the engine writes for `wav16`, and with shaped dither
//! exactly those of `flux_engine::dither::to_i16_shaped`.

use flux_assets::flac::{self, BitDepth, Dither, FlacOptions};
use flux_engine::dither::to_i16_shaped;
use flux_engine::dsp::to_i16_tpdf;

fn decode(bytes: &[u8]) -> Vec<i32> {
    let mut r = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
    r.samples().map(|s| s.unwrap()).collect()
}

fn widen(s: &[i16]) -> Vec<i32> {
    s.iter().map(|&s| i32::from(s)).collect()
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

#[test]
fn shaped_matches_engine_samples() {
    let cases = [(1usize, 1u64), (2, 1), (4095, 1), (4097, 1), (250_001, 1)];
    for (n, seed) in cases.into_iter().chain([0, 2, u64::MAX].map(|seed| (30_000, seed))) {
        let (l, r) = test_signal(n);
        let engine = to_i16_shaped(&(l.clone(), r.clone()), seed);
        let opts = FlacOptions { dither: Dither::Shaped { seed }, ..Default::default() };
        let (bytes, rep) = flac::encode_f32_stereo(44_100, &l, &r, &opts).unwrap();
        assert!(rep.verified);
        let got = decode(&bytes);
        assert_eq!(got.len(), engine.len());
        assert!(got == widen(&engine), "n = {n}, seed {seed}: FLAC samples differ from to_i16_shaped");
        let (q, _) = flac::quantize_interleaved(&[&l, &r], BitDepth::B16, Dither::Shaped { seed }).unwrap();
        assert_eq!(q, got);
        assert_eq!(rep.non_finite, [&l, &r].iter().flat_map(|c| c.iter()).filter(|x| !x.is_finite()).count() as u64);
    }
}

#[test]
fn flac_command_takes_the_dither() {
    let dir = tempfile::tempdir().unwrap();
    let (l, r) = test_signal(20_000);
    let wav = dir.path().join("in.wav");
    let float = hound::SampleFormat::Float;
    let spec = hound::WavSpec { channels: 2, sample_rate: 44_100, bits_per_sample: 32, sample_format: float };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    for (a, b) in l.iter().zip(&r) {
        w.write_sample(*a).unwrap();
        w.write_sample(*b).unwrap();
    }
    w.finalize().unwrap();
    let out = dir.path().join("out.flac");
    let run = |extra: &[&str]| -> anyhow::Result<Vec<i32>> {
        let mut args = vec![wav.display().to_string(), out.display().to_string()];
        args.extend(extra.iter().map(|s| s.to_string()));
        flux_assets::cli::run_flac(&args, &mut flux_assets::workspace::RecordingUi::default())?;
        Ok(decode(&std::fs::read(&out)?))
    };
    let x = (l.clone(), r.clone());
    assert_eq!(run(&[]).unwrap(), widen(&to_i16_tpdf(&x, 1)));
    assert_eq!(run(&["--dither", "tpdf", "--seed", "9"]).unwrap(), widen(&to_i16_tpdf(&x, 9)));
    assert_eq!(run(&["--dither", "shaped"]).unwrap(), widen(&to_i16_shaped(&x, 1)));
    assert_eq!(run(&["--dither=shaped", "--seed", "9"]).unwrap(), widen(&to_i16_shaped(&x, 9)));
    let none = flac::quantize_interleaved(&[&l, &r], BitDepth::B16, Dither::None).unwrap().0;
    assert_eq!(run(&["--dither", "none"]).unwrap(), none);
    assert_eq!(run(&["--no-dither"]).unwrap(), none);
    assert_eq!(run(&["--no-dither", "--dither", "none"]).unwrap(), none);
    // 24-bit shaped is 24-bit TPDF
    assert_eq!(run(&["--bits", "24", "--dither", "shaped"]).unwrap(), run(&["--bits", "24"]).unwrap());
    for bad in [&["--dither", "triangular"][..], &["--dither"], &["--no-dither", "--dither", "shaped"]] {
        assert!(run(bad).is_err(), "{bad:?} should be refused");
    }
}
