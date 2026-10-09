# flux-nam

Guitar and bass rigs for Flux: rig files, Neural Amp Modeler (`.nam`) inference, cabinet impulse responses and
the rig insert chain. It replaces `daw/bridge/rig.py` (Python, pedalboard, PyTorch) with a streaming,
allocation-free Rust implementation that reproduces it.

A rig (`sfz/rigs/*.rig.json`) is an SFZ source plus an insert chain:

```json
{
  "source": {"sfz": "instruments/.../01-METAL-GTX Full.sfz", "init_cc": {"22": 51}, "keyswitches": [[0, 29]],
             "tune_cents": -5, "rr_offset": 3, "default_ks": 17},
  "chain": [
    {"type": "highpass", "hz": 45},
    {"id": "amp", "type": "nam", "model": "instruments/nam/x/model_a2.nam", "input_db": 0, "output_db": -6},
    {"type": "ir", "path": "cab.wav", "mix": 1.0},
    {"type": "peak", "hz": 2500, "db": -2, "q": 1.0},
    {"type": "comp", "threshold_db": -18, "ratio": 3, "attack_ms": 10, "release_ms": 120},
    {"type": "gain", "db": -3}
  ]
}
```

Stage types: `highpass`, `lowpass` (`hz`), `peak`, `lowshelf`, `highshelf` (`hz`, `db`, `q`), `gain` (`db`),
`gate`, `comp` (`threshold_db`, `ratio`, `attack_ms`, `release_ms`), `ir` (`path`, `mix`), `nam` (`model`,
`input_db`, `output_db`, `size`) and `vst3` (`path`, `params`). Defaults, validation and the job-parameter
overrides (`"stage.field": value`, matched by `id` first, then by `type`) follow `rig.py`; unknown fields are
errors. See the `rig` module documentation for the full format.

## Usage

```rust
use flux_nam::{NoHooks, Rig, RigChain};
use serde_json::json;

let rig = Rig::load("sfz/rigs/gtx_5150_L.rig.json", ".")?;          // relative paths resolve against the root
let params = [("nam.input_db", json!(3.0))];
let rig = rig.with_overrides(params.iter().map(|(k, v)| (*k, v)))?;  // job parameters
let mut chain = RigChain::new(&rig, 48000, 512, &mut NoHooks)?;     // loads models and IRs
let mut block = vec![0.0f32; 512];                                   // mono output of the SFZ source
chain.process(&mut block);                                           // in place, real-time safe
let delay = chain.latency();                                         // samples, for latency compensation
chain.reset();                                                       // before the next render
```

The source side is engine-neutral: `source_events(&rig.source, &notes, &ccs, &bends, sample_rate)` returns the
events to play into the SFZ engine (initial controllers, the round-robin pre-roll, keyswitch ordering, exactly as
`rig.py` sends them to sfizz) and the pre-roll length to discard. The chain input is the source's mono signal
(the mean of its left and right channels, as `rig.py` does).

### Main API

| Item | Purpose |
| --- | --- |
| `Rig::load(path, root) -> Result<Rig>` | Read, validate, check referenced files exist. |
| `Rig::from_json_str(text, root, label)`, `Rig::parse(&Value, root, label)` | Without file access. |
| `rig.with_overrides(params) -> Result<Rig>` | `params: IntoIterator<Item = (impl AsRef<str>, &Value)>`. |
| `source_events(&Source, &[JobNote], &[JobCc], &[JobBend], u32) -> SourceEvents` | Events for the SFZ source. |
| `RigChain::new(&rig, sample_rate, max_block, &mut dyn StageHooks) -> Result<RigChain>` | Build the chain. |
| `RigChain::with_options(.., ChainOptions)` | Choose resampling filters and A2 submodels. |
| `chain.process(&mut [f32])`, `latency()`, `reset()`, `stage_latencies()` | Streaming processing. |
| `chain.process_offline(&[f32]) -> Vec<f32>` | Whole signal, latency compensated (allocates). |
| `NamModel::load(path, SubmodelChoice)`, `model.instance() -> NamNet` | NAM inference on its own. |
| `NamStage`, `Resampler`, `Convolver`, `Iir`, `Gate`, `Compressor` | Building blocks, public for reuse. |

`RigChain` is `Send`. Errors (`flux_nam::Error`) name the file and the JSON location, for example
`sfz/rigs/x.rig.json: chain[1] (nam).input_db: expected a finite number, got the string "3"`.

### VST3 stages and other host services

This crate hosts no plugins. `RigChain::new` asks a `StageHooks` implementation for a `Box<dyn MonoProcessor>`
for each `vst3` stage; `NoHooks` refuses with a clear error (`...: chain[3] (vst3): no VST3 host available to run
...`). `StereoAdapter` turns a `StereoProcessor` into a mono one the way `rig.py` runs plugins (the mono signal on
both channels, the output averaged). `StageHooks::nam_model` lets the host share loaded models (they are cheap to
clone) instead of reading a file per chain.

## Options: reference or quality

`ChainOptions::default()` and `ChainOptions::reference()` differ only where a sample-rate conversion is needed:

| | default | `reference()` (closest to `rig.py`) |
| --- | --- | --- |
| NAM capture rate differs from the host rate | `Quality::High` resampler | `Quality::Scipy` (scipy's `resample_poly` filter) |
| IR recorded at another rate | `IrResampling::High` | `IrResampling::Juce` (JUCE's `ResamplingAudioSource`) |

`Quality::Scipy` passes content just above the lower Nyquist frequency at -6 dB (aliasing) and has images and
aliases at -55 to -75 dB; `Quality::High` is flat to 20 kHz (-0.9 dB at 20.5 kHz) with aliases at -100 dB or
less. JUCE's IR resampling interpolates linearly and low-passes the IR with a second-order Butterworth filter at
the lower Nyquist frequency, losing several dB in the top octave. The repository's rigs use 48 kHz captures and no
IRs, so at 48 kHz both option sets give identical results.

## Fidelity to rig.py

Measured with the scripts in `build/flux-nam_check/` (Windows, x86-64). Differences are relative to the
reference signal.

Per stage, against `rig._stage` (pedalboard 0.9.26 for everything but NAM):

| Stage | Result |
| --- | --- |
| `highpass`, `lowpass`, `peak`, `lowshelf`, `highshelf` | bit-identical (JUCE float design and filter, including its snap-to-zero every 8192 samples) |
| `gain`, `gate`, `comp` | bit-identical |
| `ir` at the IR's own rate | -124 to -133 dB rms (float FFT rounding) |
| `ir` at another rate, `IrResampling::Juce` | -131 dB (44.1 kHz IR at 48 kHz), -150 dB (48 kHz stereo IR at 44.1 kHz) |
| `ir` at another rate, `IrResampling::High` | -15.8 dB and -41.2 dB: a different (flat) resampler, by design |
| `nam`, 9 captures at 48 kHz | -113 to -126 dB rms, peak -100 to -116 dB |
| `nam` at 44.1 kHz, `Quality::Scipy` | -115 to -124 dB rms (excluding the last 10 ms, see below) |
| `nam` at 44.1 kHz, `Quality::High` | -50 to -57 dB rms (different anti-aliasing filter, by design) |

NAM inference alone, against PyTorch (`nam` package) on 19 synthetic models covering every supported feature
(WaveNet A1/A2 with gating, blending, FiLM, groups, head 1x1, bottleneck, output heads; LSTM; Linear;
SlimmableContainer): largest error 6.99e-8 to 1.06e-6 of the output peak, for block sizes 1 to the whole signal.

End to end, `makima_control` (16 s windows of Gtr L, Gtr R with its round-robin pre-roll, Lead): source events
identical to `rig.py` at 44.1 and 48 kHz; chain output against `rig.py` (reference about -22 dBFS rms):

| Rate, options | rms difference | peak difference | correlation |
| --- | --- | --- | --- |
| 48 kHz (either) | -123.4 to -125.8 dB | -107.0 to -110.2 dB | 1.000000000 |
| 44.1 kHz, `reference()` | -123.0 to -125.2 dB | -99.3 to -108.3 dB | 1.000000000 |
| 44.1 kHz, default | -53.9 to -57.1 dB | -35.1 to -46.4 dB | 0.99999834 to 0.99999911 |

With resampling, the last few milliseconds differ more (`reference()`: -89 to -102 dB rms, -45 to -58 dB peak when
the last 10 ms are included): `rig.py` treats the model output as ending with the signal, while the stream keeps
the model's response to the resampling filter's tail.

## Latency

Only resampled NAM stages (and host-supplied processors) add latency. A NAM stage whose capture rate differs from
the host rate delays by the smallest amount that keeps the stream causal: 128 samples (2.9 ms) at 44.1 kHz with a
48 kHz capture and `Quality::High`, 20 samples with `Quality::Scipy`. `RigChain::latency()` is the sum; the
convolution has none (it transforms each partial block).

## Real-time safety

`RigChain::process` and `reset` never allocate, lock, block or do I/O (an integration test checks this with a
counting global allocator for every built-in stage type, with and without resampling, at block sizes 1 to 1000).
All buffers are sized in `RigChain::new` from `max_block` (at most 65536; longer calls are split). NaN and
infinite input samples are replaced like `np.nan_to_num` before the stages, and the output is cleaned the same
way. Model and IR loading, resampler design and FFT planning happen only in `RigChain::new`.

Filters keep JUCE's behaviour of flushing their state to zero only every 8192 samples; during silence the state
can become subnormal in between. Hosts should enable flush-to-zero / denormals-are-zero on their audio threads
(which changes results only below 1e-38).

## Performance

AMD Ryzen 7 9850X3D, the `jp-5150-mesa-fullrig` capture (`0ff5x0ot7r6c_a2.nam`, WaveNet, receptive field 6347,
about 11.6k multiply-adds per sample), 48 kHz, 512-sample blocks, 10 s of audio
(`cargo run -p flux-nam --release --example nam_bench -- <model.nam> [--instances N] [--rate HZ]`):

| Build | 1 instance | 4 instances (one thread) | worst block |
| --- | --- | --- | --- |
| `--release` | realtime factor 0.011 to 0.012 (about 85x realtime) | 0.045 to 0.048 total | 3 % (1 instance) to 10 % (4) of its duration |
| dev profile (`opt-level = 3`, debug assertions on) | 0.018 (55x) | 0.073 total | 16 % |
| `--release`, 44.1 kHz host (resampling, `Quality::High`) | 0.0134 | | 3 % |

The NAM convolutions use AVX2/FMA kernels when the CPU has them (runtime detection) and a portable path
otherwise. A whole repository rig chain (high-pass and NAM) processes 16 s of audio in about 0.2 s in release
builds, including resampling at 44.1 kHz.

## Verification scripts

In `build/flux-nam_check/` (run with the repository's virtual environment, `.venv/Scripts/python -I <script>`):

* `make_fixtures.py <out_dir>`: the synthetic `.nam` models and PyTorch outputs in `tests/fixtures`;
* `e2e_check.py <root> <examples_dir> <work_dir>`: the end-to-end comparison above;
* `stage_check.py <root> <examples_dir> <work_dir> <dry_dir>`: the per-stage comparison;
* `resample_check.py <examples_dir> <work_dir>`: the resampler against `resample_poly` and tone measurements;
* `probe_filters.py`, `probe_ir.py`: emulations of JUCE's float filters and IR resampling, compared with
  pedalboard (`probe_ir.py --fixture` writes `tests/fixtures/ir_juce.*`).

`<examples_dir>` holds the release examples (`cargo build -p flux-nam --release --examples`): `rig_process`,
`rig_events`, `nam_run`, `nam_bench`, `resample`.

## Limitations

* `.nam` features without a Python reference are refused with a clear error: `condition_dsp`, `packing`,
  `ConvNet` and other architectures. PReLU activations are implemented but not verified against the Python
  package (no fixture covers them).
* `rig.py` runs captures in 8 s chunks, each starting from the model's initial state with a 16384-sample lead-in.
  That is seamless for WaveNets (receptive fields are shorter) but not exactly for LSTMs, whose state the stream
  carries across the whole signal. No repository capture is an LSTM.
* Filter coefficients use the platform's `sinf`/`cosf`/`tanf`/`powf`; bit-identical results with pedalboard are
  verified on Windows (MSVC runtime). Other platforms may differ in the last bit of a coefficient.
* Debug-assertion builds are slower than release for the element-wise parts of inference.

## Licences and credits

* NAM inference follows the `neural-amp-modeler` Python package (MIT, Steven Atkinson) and
  NeuralAmpModelerCore (MIT, Steven Atkinson); no code is copied, the network definitions and file format are
  re-implemented from them.
* The filter, gate, compressor, IR normalisation and IR resampling behaviour reproduce JUCE (via pedalboard) and
  scipy's `resample_poly` filter design from their documented algorithms; no code is copied.
* Dependencies: `realfft` (MIT), `hound` (Apache-2.0), `serde`, `serde_json`, `thiserror`, `anyhow` (MIT or
  Apache-2.0).
* NAM captures (`instruments/nam/`) are TONE3000 (T3K) licensed and are not part of this crate; tests that use
  them skip when they are absent.
