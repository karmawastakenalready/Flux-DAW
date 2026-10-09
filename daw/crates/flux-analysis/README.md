# flux-analysis

Measurements on Flux's rendered audio, for an agent that cannot listen: is every track of the game at the
target loudness and seamless as a loop, how does a mix differ from a reference, which parts mask each other,
which sections are muddy or harsh, does reverb blur the attacks, does the mix survive mono. It also holds the
Rust ports of the Python measurement tools (`tools/spectro.py`, `tools/stem_balance.py`,
`tools/time_fx_check.py`, `tools/instrument_card.py` and the loudness/seam checks of `verify.py`).

Every analysis takes an `AudioRef`, a borrowed planar stereo view, so it runs in place on a render
(`flux_engine::RenderOutput`: the master and its layer stems) or on a decoded WAV or FLAC file. Results are
plain structs that serialise to JSON; every finding carries a stable `code` or `area` and a message that says
what to change.

## Modules

| Module | What it answers |
|---|---|
| `loudness` | Game-wide check: integrated loudness, loudness range, true peak, short-term and momentary maxima, PLR, clipping and file size per track against the target (-15 LUFS +/- 1, -1 dBTP, 20 MB); tracks that will jump out next to the others; loop seams; layered exports (missing, stale or unlisted layers, sum against the master). |
| `reference` | A mix against a reference track at equal loudness, whole and per section: ranked suggestions on band balance (naming the stems that carry the band), dynamics, transients, width, low end and mono. |
| `mix` | Masking between stems per band and section (collisions, doubles, buried parts, with which stem to cut), mud and harshness per section and the stems causing them, reverb smearing (wet against a dry render), mono compatibility. |
| `profile` | The measurements behind both: loudness and dynamics, onsets and transient clarity, 1/3-octave balance, per-octave width, tone (mud, harshness, air, centroid) and mono fold-down. |
| `spectro` | Log-frequency spectrogram PNGs, one band per signal. |
| `stems` | The stem balance table: RMS of each stem per section. |
| `timefx` | Time-stop envelope (level and spectral centroid) and the played tempo per block of bars. |
| `card` | Instrument report cards: render an SFZ through sfizz and measure range, tuning, levels, timbre, dynamics, articulations and license. |
| `meta` | Sections, tempo maps, Flux's layered-export manifest (`<stem>.layers.json`) and the pipeline's song metadata (`output/<title>/<title>.json`). |
| `spectral`, `num`, `json` | Shared signal processing (STFT, stereo spectra, K-weighting, 1/3-octave bands, numpy-compatible numerics) and the card JSON writer. |

## Library

```rust
use flux_analysis::loudness::{GameInput, GameOptions, check_game, check_game_audio};
use flux_analysis::meta::Section;
use flux_analysis::mix::{MixOptions, check_mix};
use flux_analysis::reference::{ReferenceOptions, compare};
use flux_analysis::{Audio, AudioRef};

// a render, checked as one track of the game (bar lines and layers come with it)
let report = check_game_audio(vec![GameInput::from_render("capital_city", &render)], &GameOptions::default())?;

// files and directories (WAV, FLAC, .layers.json); manifests claim their layer files
let report = check_game(&["output/".into()], &GameOptions { recursive: true, ..Default::default() })?;
for t in &report.tracks {
    for f in &t.flags {
        println!("{} [{} {}] {}", t.name, f.severity, f.code, f.message);
    }
}

// mix checks on a render, its layers as stems, per section
let master = AudioRef::master(&render);
let stems = AudioRef::layers(&render);
let mix = check_mix(master, &stems, None, &MixOptions { sections: Section::of_render(&render), ..Default::default() })?;

// reference matching
let reference = Audio::read(std::path::Path::new("refs/target.flac"))?;
let rep = compare(master, reference.view(), &stems, &ReferenceOptions::default())?;
```

## Command line

`flux-analyze` runs the analyses on files and prints JSON (`--text` for readable tables); see
`src/bin/flux-analyze.txt` or `flux-analyze --help`.

```
flux-analyze loudness output/ --recursive            # the game; exit code 1 when a track has an error
flux-analyze mix song.wav --manifest song.layers.json [--dry song_dry.wav]
flux-analyze reference song.wav refs/target.flac --manifest song.layers.json --sections song.layers.json
flux-analyze stems --meta output/<title>/<title>.json --text
flux-analyze timefx --meta output/<title>/<title>.json --text
flux-analyze spectro out.png 30 45 a.wav b.wav
flux-analyze profile song.wav --sections sections.json
```

Exit codes: 0 done, 1 `loudness` found an error flag or an unreadable file, 2 bad arguments or unreadable
input (message on stderr).

`flux-card` writes instrument report cards (`flux-card --help`); by default each card runs in a child process,
so a sfizz crash costs only that card.

## Loudness flags

| Code | Severity | Meaning |
|---|---|---|
| `silent` | error | No gated loudness. |
| `loudness_target` | error | Integrated loudness off the target by more than the tolerance (the message gives the gain change). |
| `true_peak` | error | Over the ceiling. |
| `clipped` | error | Samples at full scale. |
| `file_size` | error | Over the upload limit (20 000 000 bytes by default). |
| `duration` | error | A manifest's master is not as long as the manifest says. |
| `seam_jump` | error | The wrap-around step of a loop is larger than 99.9 % of the track's own sample steps. |
| `layer_unusable` | error | A layer file is missing, unreadable, or differs in rate or length from the master. |
| `layer_sum` | error | The layers do not sum to the master (beyond quantisation noise): a layer is missing or out of date. |
| `layer_stale` | warning | A layer measures more than 0.3 LU away from its manifest entry. |
| `seam_click`, `seam_level_step` | warning | The seam is clearly (6 dB) more abrupt than the song's own bar lines, or its own attacks when bar times are unknown. |
| `loud_passage` | warning | The loudest 3 s are more than 8 LU over the track's integrated loudness. |
| `over_limited` | warning | Peak-to-loudness ratio under 6 dB. |
| `loudness_outlier`, `peak_loudness_outlier`, `dynamics_outlier`, `sample_rate` | warning | The track differs from the rest of the game (1.5 LU from the median, loudest 3 s 3 LU over the median, loudness range far from the median, a sample rate most tracks don't use). |

Loops are measured as they play: the meter's filters are warmed with the end of the loop and the short-term,
momentary and true-peak measurements see the wrap. A file is a loop when Flux's export report beside it
(`<name>.report.json`) says so, else when the pipeline's song metadata (`<title>.json`, same loop length) does,
else when its name contains "loop"; manifests say so themselves.

## Verification

All against the Python tools on the project's own renders (scripts and data in `build/flux-analysis_check/`):

* **Loudness** (7 masters): integrated, LRA, short-term and momentary maxima, sample peak, RMS and crest match a
  numpy BS.1770 / EBU Tech 3342 implementation with libebur128's filter coefficients to 0.0000. pyloudnorm reads
  0.045 LU lower (its K-weighting coefficients are 0.043 dB off the ITU table). True peak is within 0.025 dB of
  4x `resample_poly`.
* **Loop seams** (4 loops) against `verify.py`: sample jump, 99.9th-percentile step and level step identical; HF
  click within 0.03 dB; the bar-line references identical.
* **Synthesised game** (12 tracks with known defects): every expected flag found, clean tracks clean.
* **Stem balance** (3 songs, 12-20 stems): identical tables; largest unrounded difference 1.5e-5 dB.
* **Time effects** (2 songs): identical text output; levels within 1.9e-6 dB, tempo within 5e-7 BPM.
* **Spectrograms**: pixel-identical PNGs.
* **Instrument cards** (56 instruments): every field of the reference schema identical except the run time and
  date; the catalog README identical.

Thresholds for mud, harshness and doubles were calibrated on finished mixes of the project: harshness 2-5 kHz
against pink noise sits between -6 and +2.5 dB in mixes heard as fine and between +3.6 and +4.3 dB in the
glockenspiel-led sections heard as harsh (flagged above +3 dB); mud sits between -5 and +4.8 dB (flagged above
+4 dB); intended doubles correlate 0.93-1.00 in level envelope, unrelated colliding parts mostly under 0.85
(doubles from 0.9).

## Limitations

* The mud, harshness and double thresholds are heuristics calibrated on a handful of songs.
* Reverb smearing needs a dry render, which `flux-engine` cannot produce yet; without it only the wet values are
  reported.
* `stems` measures stems as they sound in the mix (Flux renders post-fader); it does not re-run the Python
  pipeline's mix chain on raw stems.
* Loudness range uses libebur128's short-term blocks every second (EBU Tech 3342 allows any hop; a 100 ms hop
  differs by up to 0.4 LU on the project's songs, inside the specification's tolerance).
* Instruments with random opcodes reproduce the Python cards only when carded in a child process (the default).
