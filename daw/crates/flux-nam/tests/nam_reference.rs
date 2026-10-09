//! NAM inference against PyTorch: the fixtures in `tests/fixtures` are random-weight models and the output the
//! `neural-amp-modeler` package computes for them (`build/flux-nam_check/make_fixtures.py`).

use flux_nam::{NamModel, SubmodelChoice};
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

fn read_f32(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// Largest absolute difference relative to the reference's peak.
fn rel_error(got: &[f32], want: &[f32]) -> f64 {
    assert_eq!(got.len(), want.len());
    let peak = want.iter().fold(0.0f64, |m, &v| m.max(v.abs() as f64));
    let err = got.iter().zip(want).fold(0.0f64, |m, (&a, &b)| m.max((a as f64 - b as f64).abs()));
    err / peak.max(1e-6)
}

fn run(model: &NamModel, input: &[f32], block: usize) -> Vec<f32> {
    let mut net = model.instance();
    let mut y = input.to_vec();
    for part in y.chunks_mut(block) {
        net.process(part);
    }
    y
}

fn cases() -> Vec<String> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures().join("manifest.json")).expect("manifest")).expect("json");
    manifest["cases"].as_array().expect("cases").iter().map(|c| c["name"].as_str().expect("name").to_owned()).collect()
}

/// Relative error allowed: float32 arithmetic in a different order than PyTorch's, and the fast tanh / sigmoid
/// (a few ulp). Observed errors are far below this.
const TOLERANCE: f64 = 2e-5;

#[test]
fn matches_pytorch_for_every_fixture() {
    let input = read_f32(&fixtures().join("input.f32"));
    let mut report = vec![];
    for name in cases() {
        let model = NamModel::load(fixtures().join(format!("{name}.nam")), SubmodelChoice::Largest)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let want = read_f32(&fixtures().join(format!("{name}.out.f32")));
        let mut worst = 0.0f64;
        for block in [1, 5, 16, 128, 300, input.len()] {
            let got = run(&model, &input, block);
            let err = rel_error(&got, &want);
            assert!(err < TOLERANCE, "{name} (block {block}): relative error {err:.3e}");
            worst = worst.max(err);
        }
        // reset returns to the initial state exactly
        let mut net = model.instance();
        let mut first = input.clone();
        net.process(&mut first);
        net.reset();
        let mut again = input.clone();
        net.process(&mut again);
        assert_eq!(first, again, "{name}: reset");
        report.push(format!("{name:30} {:>8} rf {:>5}  max error / peak {worst:.2e}", model.architecture(), model.receptive_field()));
    }
    println!("{}", report.join("\n"));
}

#[test]
fn container_submodel_choice() {
    let input = read_f32(&fixtures().join("input.f32"));
    let path = fixtures().join("container.nam");
    let largest = NamModel::load(&path, SubmodelChoice::Largest).unwrap();
    let info = largest.submodel().unwrap();
    assert_eq!((info.index, info.max_value, info.available.clone()), (1, 1.0, vec![0.5, 1.0]));
    assert_eq!(largest.sample_rate(), 44100);
    let small_want = read_f32(&fixtures().join("container.sub0.out.f32"));
    for choice in [SubmodelChoice::Index(0), SubmodelChoice::Size(0.3), SubmodelChoice::Size(0.5)] {
        let m = NamModel::load(&path, choice).unwrap();
        assert_eq!(m.submodel().unwrap().index, 0, "{choice:?}");
        assert!(rel_error(&run(&m, &input, 64), &small_want) < TOLERANCE);
    }
    for choice in [SubmodelChoice::Size(0.75), SubmodelChoice::Size(2.0), SubmodelChoice::Index(1)] {
        assert_eq!(NamModel::load(&path, choice).unwrap().submodel().unwrap().index, 1, "{choice:?}");
    }
    let e = NamModel::load(&path, SubmodelChoice::Index(2)).unwrap_err().to_string();
    assert!(e.contains("config.submodels: has 2 submodels; index 2 does not exist"), "{e}");
}

#[test]
fn model_errors_are_located() {
    let base: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures().join("wavenet_tanh_basic.nam")).unwrap()).unwrap();
    let check = |edit: &dyn Fn(&mut serde_json::Value), want: &str| {
        let mut v = base.clone();
        edit(&mut v);
        let e = NamModel::from_json(&v, "m.nam", SubmodelChoice::Largest).unwrap_err().to_string();
        assert!(e.contains(want), "expected {want:?} in {e:?}");
    };
    check(&|v| v["weights"].as_array_mut().unwrap().truncate(10), "m.nam.weights: has 10 values, fewer than");
    check(&|v| v["weights"].as_array_mut().unwrap().push(0.5.into()), "but the configuration uses");
    check(&|v| v["weights"][3] = "x".into(), "m.nam.weights[3]: expected a finite number");
    check(&|v| v["architecture"] = "ConvNet".into(), "unsupported architecture \"ConvNet\"");
    check(&|v| v["config"]["layers"][0]["dilations"] = serde_json::json!([1, 2]), "m.nam.config.layers[0]");
    check(&|v| v["config"]["layers"][0]["packing"] = serde_json::json!({}), "packing");
    check(&|v| v["config"]["condition_dsp"] = serde_json::json!({}), "condition_dsp");
    check(&|v| v["config"]["layers"][0]["activation"][0] = serde_json::json!({"type": "Swish9"}), "Swish9");
    check(&|v| v["sample_rate"] = (-5).into(), "sample_rate");
}

/// The local A2 captures, when the repository's `instruments/nam` folder is present (they are not
/// redistributable, so the crate does not contain them).
#[test]
fn loads_the_repository_captures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../instruments/nam");
    let Ok(dirs) = std::fs::read_dir(&root) else {
        eprintln!("skipped: {} not found", root.display());
        return;
    };
    let mut n = 0;
    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().is_none_or(|e| e != "nam") {
                continue;
            }
            let m = NamModel::load(&p, SubmodelChoice::Largest).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(m.architecture(), "WaveNet");
            assert_eq!(m.sample_rate(), 48000);
            assert_eq!(m.receptive_field(), 6347, "{}", p.display());
            let mut x: Vec<f32> = (0..4800).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
            m.instance().process(&mut x);
            assert!(x.iter().all(|v| v.is_finite()) && x.iter().any(|&v| v.abs() > 1e-3), "{}", p.display());
            n += 1;
        }
    }
    eprintln!("{n} captures loaded");
}
