//! Out-of-process hosting end to end, with the plugins in the repository's `plugins/` folder
//! (Dexed and Surge XT). Tests skip when a plugin isn't there.

#![cfg(windows)]

use flux_vst3::client::{AudioError, HostOptions, LoadOptions, PluginHost};
use flux_vst3::preset::Preset;
use flux_vst3::{Midi, Module, Plugin, Setup, TimedMidi};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SR: f64 = 48000.0;

fn plugin(rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").join(rel);
    if p.exists() {
        Some(p)
    } else {
        eprintln!("skipping: {} isn't installed", p.display());
        None
    }
}

fn opts(label: &str) -> HostOptions {
    HostOptions { exe: Some(env!("CARGO_BIN_EXE_flux-plugin-host").into()), label: label.into(), ..Default::default() }
}

fn offline() -> Setup {
    Setup { sample_rate: SR, max_block: 512, offline: true }
}

fn note(pitch: u8, on: u32, off: u32) -> Vec<TimedMidi> {
    vec![
        TimedMidi { offset: on, msg: Midi::NoteOn { ch: 0, pitch, vel: 100 } },
        TimedMidi { offset: off, msg: Midi::NoteOff { ch: 0, pitch, vel: 0 } },
    ]
}

fn rms_db(x: &[f32]) -> f64 {
    let s: f64 = x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64;
    10.0 * s.max(1e-30).log10()
}

#[test]
fn dexed_matches_in_process_rendering() {
    let Some(path) = plugin("plugins/dexed/Dexed.vst3") else { return };
    let frames = 2 * SR as usize;
    let midi = note(60, 0, SR as u32);

    // in this process
    let module = Arc::new(Module::load(&path).unwrap());
    let mut p = Plugin::new(module, None, offline()).unwrap();
    let mut proc = p.processor().unwrap();
    let (mut l, mut r) = (vec![0f32; frames], vec![0f32; frames]);
    proc.process(frames, &[], &mut [&mut l[..], &mut r[..]], &midi, &[], None);
    drop(proc);
    drop(p);

    // in a plugin host
    let host = PluginHost::spawn(opts("Dexed")).unwrap();
    let (loaded, mut port) = host.load(&LoadOptions::instrument(&path, offline())).unwrap();
    assert_eq!(loaded.info.class.name, "Dexed");
    assert!(loaded.info.params > 1000, "params: {}", loaded.info.params);
    assert!(loaded.midi_mapped > 0);
    let (mut l2, mut r2) = (vec![0f32; frames], vec![0f32; frames]);
    let rep = port.process(frames, &[], &mut [&mut l2[..], &mut r2[..]], &midi, &[], None).unwrap();
    assert!(!rep.skipped);
    assert_eq!(rep.dropped, 0);

    let level = rms_db(&l2);
    eprintln!("dexed: {} params, rms {level:.1} dB, host pid {}", loaded.info.params, host.pid());
    assert!(level > -45.0, "out-of-process render is silent ({level:.1} dB)");
    let diff = l.iter().zip(&l2).chain(r.iter().zip(&r2)).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    // Dexed renders in 16-bit fixed point and isn't bit-identical between instances (one LSB, 2^-15, now and
    // then); a transport fault would show as far larger errors
    assert!(diff < 1e-4, "in-process and out-of-process renders differ by {diff}");
    host.shutdown().unwrap();
    assert!(!host.alive());
}

#[test]
fn dexed_programs_params_and_state() {
    let Some(path) = plugin("plugins/dexed/Dexed.vst3") else { return };
    let host = PluginHost::spawn(opts("Dexed")).unwrap();
    let (loaded, _port) = host.load(&LoadOptions::instrument(&path, offline())).unwrap();
    assert!(loaded.program_lists.iter().any(|l| l.programs == 32), "{:?}", loaded.program_lists);

    let lists = host.programs().unwrap();
    let name = host.set_program(lists[0].id, 5).unwrap();
    assert_eq!(name, lists[0].programs[5]);

    // a typo changes nothing and suggests names
    let err = host.set_params(&[("Cutof".into(), json!(50))]).unwrap_err().to_string();
    assert!(err.contains("no parameter named"), "{err}");

    let params = host.params(Some("cutoff")).unwrap();
    let cutoff = params.first().expect("Dexed has a cutoff parameter").clone();
    let set = host.set_params(&[(cutoff.title.clone(), json!({"normalized": 0.25}))]).unwrap();
    assert!((set[0].normalized - 0.25).abs() < 1e-6);

    // state moves between processes
    let bytes = host.state().unwrap();
    let preset = Preset::from_bytes(&bytes).unwrap();
    assert!(preset.class_id.eq_ignore_ascii_case(&loaded.info.class.cid));
    let other = PluginHost::spawn(opts("Dexed 2")).unwrap();
    let _ = other.load(&LoadOptions::instrument(&path, offline())).unwrap();
    other.set_state(&bytes).unwrap();
    let v = other.params(Some(&cutoff.title)).unwrap();
    let v = v.iter().find(|p| p.id == cutoff.id).unwrap();
    assert!((v.value - 0.25).abs() < 1e-3, "restored cutoff {}", v.value);
    assert_eq!(other.state().unwrap(), bytes);
}

#[test]
fn surge_params_resolve_human_values() {
    let Some(path) = plugin("plugins/surge-xt/Surge XT.vst3") else { return };
    let host = PluginHost::spawn(opts("Surge XT")).unwrap();
    let (loaded, mut port) = host.load(&LoadOptions::instrument(&path, offline())).unwrap();
    assert_eq!(loaded.info.class.name, "Surge XT");
    let set = host
        .set_params(&[
            ("a_filter_1_cutoff".into(), json!(900)),
            ("A Filter 1 Resonance".into(), json!(15)),
            ("a_volume".into(), json!(-6)),
        ])
        .unwrap();
    let shown: Vec<String> = set.iter().map(|s| s.display.clone().unwrap_or_default()).collect();
    assert!(shown[0].starts_with("900"), "{shown:?}");
    assert!(shown[1].starts_with("15"), "{shown:?}");
    assert!(shown[2].starts_with("-6"), "{shown:?}");

    let frames = SR as usize;
    let (mut l, mut r) = (vec![0f32; frames], vec![0f32; frames]);
    port.process(frames, &[], &mut [&mut l[..], &mut r[..]], &note(57, 100, 30000), &[], None).unwrap();
    let level = rms_db(&l);
    eprintln!("surge: {shown:?}, rms {level:.1} dB");
    assert!(level > -50.0, "Surge rendered silence ({level:.1} dB)");
}

#[test]
fn parallel_blocks_and_crash_detection() {
    let Some(path) = plugin("plugins/dexed/Dexed.vst3") else { return };
    let rt = Setup { sample_rate: SR, max_block: 256, offline: false };
    let a = PluginHost::spawn(opts("Dexed A")).unwrap();
    let b = PluginHost::spawn(opts("Dexed B")).unwrap();
    let (_, mut pa) = a.load(&LoadOptions::instrument(&path, rt)).unwrap();
    let (_, mut pb) = b.load(&LoadOptions::instrument(&path, rt)).unwrap();
    let (mut la, mut ra, mut lb, mut rb) = (vec![0f32; 256], vec![0f32; 256], vec![0f32; 256], vec![0f32; 256]);
    let mut energy = 0.0;
    for k in 0..200 {
        let midi = if k == 0 { note(64, 0, 255)[..1].to_vec() } else { vec![] };
        // both plugins work at the same time; we collect afterwards
        pa.begin(256, &[], &midi, &[], None).unwrap();
        pb.begin(256, &[], &midi, &[], None).unwrap();
        pa.end(&mut [&mut la[..], &mut ra[..]]).unwrap();
        pb.end(&mut [&mut lb[..], &mut rb[..]]).unwrap();
        energy += la.iter().map(|x| x * x).sum::<f32>();
    }
    assert!(energy > 0.0);

    // a plugin host that dies is noticed by the audio port and the control connection
    b.kill();
    pb.set_timeout(Duration::from_secs(5));
    let _ = pb.begin(256, &[], &[], &[], None);
    assert_eq!(pb.end(&mut [&mut lb[..], &mut rb[..]]).unwrap_err(), AudioError::Exited);
    assert!(lb.iter().all(|x| *x == 0.0));
    let err = format!("{:#}", b.info().unwrap_err());
    assert!(err.contains("Dexed B"), "{err}");
    // the other one is unaffected
    pa.begin(256, &[], &[], &[], None).unwrap();
    pa.end(&mut [&mut la[..], &mut ra[..]]).unwrap();
    assert!(a.alive());
}

#[test]
fn load_errors_are_reported() {
    let host = PluginHost::spawn(opts("missing")).unwrap();
    let err = format!("{:#}", host.load(&LoadOptions::instrument("C:/nope/Missing.vst3", offline())).err().unwrap());
    assert!(err.contains("Missing.vst3"), "{err}");
    // the host stays usable after a failed load
    assert!(host.alive());
    assert!(host.info().is_err());
}

/// Opens real windows: run with `cargo test -p flux-vst3 --test host -- --ignored editor`.
#[test]
#[ignore]
fn editor_opens_and_closes() {
    for rel in ["plugins/dexed/Dexed.vst3", "plugins/surge-xt/Surge XT.vst3"] {
        let Some(path) = plugin(rel) else { continue };
        let host = PluginHost::spawn(opts(rel)).unwrap();
        let _ = host.load(&LoadOptions::instrument(&path, offline())).unwrap();
        host.open_editor("Flux editor test", None).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let events = host.take_events();
        assert!(events.iter().any(|e| matches!(e, flux_vst3::ipc::HostEvent::EditorOpened)), "{events:?}");
        assert!(host.info().unwrap().editor_open);
        host.close_editor().unwrap();
        assert!(!host.info().unwrap().editor_open);
        host.shutdown().unwrap();
    }
}

#[test]
fn scanning_finds_and_caches_plugins() {
    use flux_vst3::scan::{find_bundles, Scanner, Source};
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../plugins");
    if !root.is_dir() {
        return;
    }
    let bundles = find_bundles(&[root]);
    assert!(bundles.iter().any(|b| b.ends_with("Dexed.vst3")), "{bundles:?}");
    let cache = std::env::temp_dir().join(format!("flux-scan-cache-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&cache);
    let scanner = Scanner { exe: Some(env!("CARGO_BIN_EXE_flux-plugin-host").into()), cache: Some(cache.clone()), deep: true, ..Default::default() };
    let t = std::time::Instant::now();
    let found = scanner.scan(&bundles);
    let first = t.elapsed();
    for p in &found {
        eprintln!("{} ({}) {:?} instrument={} {:?}", p.name(), p.vendor(), p.source, p.is_instrument(), p.error);
        assert!(p.ok(), "{}: {:?}", p.path.display(), p.error);
        assert_eq!(p.source, Source::Deep);
        assert!(p.instances.iter().all(|i| i.error.is_none() && i.info.is_some()), "{:?}", p.instances);
    }
    let dexed = found.iter().find(|p| p.name() == "Dexed").expect("Dexed scanned");
    assert!(dexed.is_instrument());
    assert!(dexed.instances[0].has_editor);
    assert_eq!(dexed.instances[0].programs, 32);
    // second time from the cache
    let t = std::time::Instant::now();
    let again = scanner.scan(&bundles);
    assert_eq!(again, found);
    assert!(t.elapsed() < first, "cache wasn't used ({:?} vs {first:?})", t.elapsed());
    let _ = std::fs::remove_file(&cache);
}

/// Mean absolute difference in dB between two signals' spectra in 24 bands (40 Hz - 12 kHz), each the
/// average of 12 Goertzel bins. Phase-insensitive and smooth, so patches with free-running oscillators
/// and LFOs (which never render the same samples twice) compare.
fn spectral_distance_db(a: &[f32], b: &[f32]) -> f64 {
    let power = |x: &[f32], hz: f64| {
        let w = 2.0 * std::f64::consts::PI * hz / SR;
        let (c, mut s1, mut s2) = (2.0 * w.cos(), 0f64, 0f64);
        for (i, v) in x.iter().enumerate() {
            let win = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / x.len() as f64).cos();
            let s0 = *v as f64 * win + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    };
    let (bands, per) = (24, 12);
    let hz = |k: f64| 40.0 * (12000.0f64 / 40.0).powf(k / (bands * per) as f64);
    let band_db = |x: &[f32], band: usize| {
        let e: f64 = (0..per).map(|j| power(x, hz((band * per + j) as f64))).sum::<f64>() / per as f64;
        10.0 * e.max(1e-20).log10()
    };
    (0..bands).map(|i| (band_db(a, i) - band_db(b, i)).abs()).sum::<f64>() / bands as f64
}

#[test]
fn program_changes_reach_the_audio_and_the_state() {
    let Some(path) = plugin("plugins/dexed/Dexed.vst3") else { return };
    let frames = SR as usize;
    let render = |port: &mut flux_vst3::client::AudioPort| {
        let (mut l, mut r) = (vec![0f32; frames], vec![0f32; frames]);
        port.process(frames, &[], &mut [&mut l[..], &mut r[..]], &note(60, 0, 30000), &[], None).unwrap();
        l
    };
    let host = PluginHost::spawn(opts("Dexed")).unwrap();
    let (_, mut port) = host.load(&LoadOptions::instrument(&path, offline())).unwrap();
    let first = render(&mut port);

    // right after a state restore, which Dexed answers by ignoring program changes for about 2 s
    host.set_state(&host.state().unwrap()).unwrap();
    let lists = host.programs().unwrap();
    host.set_program(lists[0].id, 1).unwrap();
    // the state reflects the program straight away, before the client sends any audio
    let state = host.state().unwrap();
    host.reset().unwrap();
    let second = render(&mut port);
    // this patch has free-running oscillators/LFOs: no two renders are identical, so compare spectra
    // against how much it varies by itself
    host.reset().unwrap();
    let noise = spectral_distance_db(&second, &render(&mut port));
    let d = spectral_distance_db(&first, &second);
    eprintln!("program 0 vs 1: {d:.1} dB (program 1 against itself: {noise:.1} dB)");
    assert!(d > 6.0 && d > 5.0 * noise, "the program change didn't change the sound ({d:.1} dB)");

    let other = PluginHost::spawn(opts("Dexed 2")).unwrap();
    let (_, mut port2) = other.load(&LoadOptions::instrument(&path, offline())).unwrap();
    other.set_state(&state).unwrap();
    other.reset().unwrap();
    let restored = render(&mut port2);
    let d = spectral_distance_db(&second, &restored);
    eprintln!("restored vs program 1: {d:.1} dB");
    assert!(d < 2.0f64.max(1.5 * noise), "the captured state doesn't reproduce the program ({d:.1} dB)");
}

#[test]
fn first_block_right_after_loading() {
    // the host must answer a block sent the moment the load is answered (its audio thread may not have
    // started running yet)
    let Some(path) = plugin("plugins/dexed/Dexed.vst3") else { return };
    for _ in 0..12 {
        let host = PluginHost::spawn(opts("Dexed")).unwrap();
        let (_, mut port) = host.load(&LoadOptions::instrument(&path, offline())).unwrap();
        port.set_timeout(Duration::from_secs(10));
        let (mut l, mut r) = (vec![0f32; 64], vec![0f32; 64]);
        port.process(64, &[], &mut [&mut l[..], &mut r[..]], &[], &[], None).unwrap();
    }
}
