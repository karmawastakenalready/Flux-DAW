# flux-sfz

Flux's SFZ toolkit. The crate has five parts:

- a parser and model that read SFZ files the way sfizz 1.2.3 does;
- the instrument wrappers the songs use, built from set definitions with a manifest, so builds are
  incremental and stale files are detected;
- the instrument tools: tighten, retune, even, ranges, path checks and sample summaries;
- a compact description of an instrument for an agent;
- a writer that gives every file back byte for byte.

The tools never change a library in place. Each one makes a *derived* copy that points back at the
original samples. The copy is returned as `OutputFile`s, and the caller decides where to write them
(`edit::write_files`).

## Modules

| module | what it does |
|---|---|
| `syntax` | The lossless `Document`. Comments, spacing, line ends, BOM and encoding (UTF-8, Latin-1 fallback) are kept, so `to_bytes()` returns the input exactly. Edits re-render only the items they touch. |
| `instrument` | The `Instrument` model. See "sfizz compatibility" below for the rules it follows. |
| `props` | `RegionProps`: the interpreted opcodes. It covers keys, velocities and crossfades, keyswitches, triggers, round robins, CC conditions, loop mode, envelope, tune and volume. Defaults, aliases and value parsing follow sfizz. |
| `opcode`, `value` | Opcode names (aliases, CC patterns, which opcodes sfizz supports), note names (C4 = 60) and number parsing (leading prefixes, as sfizz reads them). |
| `edit` | `Derived` copies, in two layouts: inline (one file, includes inlined) or mirror (included files copied alongside). Sample paths are relocated through `default_path` prefixes, relative by default. Region and opcode edits map back to the source text. |
| `set`, `wrap`, `manifest` | Instrument sets (`sfz/sets/<set>.toml`), wrapper generation (`expr`, `strings`, `keyswitch`, `patch`, `none`), and the incremental build and check. |
| `tighten` | A short release, and no release-triggered regions (for palm-muted parts through high-gain amps). |
| `retune` | Measures each sample against its key and writes `tune=` corrections. Samples it can't trust (release noise, harmonics that disagree) are left as they are, and the report says why. |
| `even` | Plays every key through sfizz and brings each key to the median level of its velocity layer. |
| `ranges` | Keys, key centres, keyswitches and their names, velocity layers, round robins, release regions and controllers. |
| `audit` | Missing samples, case mismatches (files that would fail on a case-sensitive system), includes that were not read, and absolute paths. |
| `samples`, `audio` | WAV/FLAC headers (format, rate, depth, length, `smpl` loops and root key) and summaries per instrument or per folder. |
| `pitch` | f0 estimate (the same algorithm as `tools/sample_pitch.py`, checked on 464 real samples) and the tuning measurement `retune` uses: the median deviation of up to ten harmonics, with a check that they stand out from the noise and agree. |
| `info` | `describe(&inst)`: everything an agent needs before writing a part, as JSON or as a few lines of text. |

## sfizz compatibility

These rules were checked against sfizz 1.2.3:

- **Includes.** `#include` paths are relative to the root file's folder, not the including file. A cycle stops
  the include, and includes nest at most 32 deep.
- **Variables.** `#define $VAR` is expanded where the variable is used, so a later redefinition applies to
  the lines after it.
- **default_path.** It is joined to `sample` as plain text, and each `<control>` block resets it.
- **Case.** Opcode names are case-insensitive, but headers are case-sensitive.
- **Values.** Values may contain spaces; a value ends at the next opcode, header, comment or directive. Keys
  accept note names. A key outside 0..127 falls back to the opcode's default.
- **Missing samples.** A region whose sample is missing is dropped, as sfizz does. `Region::is_playable` is
  false for it.

All 1754 SFZ files of the repository round-trip byte for byte (`tests/corpus.rs`, which is ignored by
default and needs the libraries).

The model was also compared with sfizz's own dump of the 171,193 regions it loads, with no differences.
The comparison covered:

- the sample, the key and velocity ranges, and the key centre;
- the trigger, the keyswitch, the round robin, `group` and `off_by`;
- the loop mode, volume, pan, width, pitch and amplitude;
- unknown opcodes, and the CC and key labels.

## Wrapper sets

A set is a TOML file. The `set` module documentation describes every field.

```toml
[set]
description = "Capital City"
# output = "build/sfz/capital_city"   (the default)

[instruments.violins]
library = "VSCO-2-CE"
wrap = "keyswitch"                     # one articulation per keyswitch, from 120 up, with sw_label names
articulations = [
    { name = "sus", source = "instruments/VSCO-2-CE/ViolinEnsSusVib.sfz", crossfade = true },
    { name = "pizz", source = "instruments/VSCO-2-CE/ViolinEnsPizz.sfz" },
]
range = [55, 86]
```

```rust
use flux_sfz::manifest::{build, check, BuildOptions, CheckOptions};
use flux_sfz::set::SetFile;

let set = SetFile::load(Path::new("sfz/sets/capital_city.toml"), root)?;
let report = build(&set, &BuildOptions::default())?;   // writes only what changed, then <output>/<set>.manifest.json
let status = check(&set, CheckOptions { regenerate: true })?;
assert!(status.is_clean());
```

The manifest records the generator version, a recipe hash for each instrument (its definition and the
generator), and the SHA-256 of every input and output.

- `build` skips instruments that are up to date and writes files atomically.
- It refuses to overwrite an output that was edited by hand, unless `force` is set.
- It deletes outputs that are no longer made, but only when they are unmodified.
- `check` reports any instrument that is not built, has a changed recipe, has changed or missing inputs, or
  has missing, modified or different outputs.
- `untracked` lists files in the output folders that no set makes.

## Tools

```rust
use flux_sfz::{Instrument, edit::write_files};
use flux_sfz::tighten::{tighten, TightenOptions};

let inst = Instrument::load("instruments/karoryfer.growlybass/growlybass_dirty.sfz")?;
let opts = TightenOptions { release_s: 0.04, note: Some("Growlybass by Karoryfer Samples, CC0.".into()), sample_paths: Default::default() };
let (files, report) = tighten(&inst, Path::new("sfz/cold_metal_growly_dirty.sfz"), &opts)?;
write_files(&files)?;            // report: release_regions_removed, release_opcodes_changed, regions_kept
```

`retune` and `even` work the same way: `(instrument, output path, options) -> (files, report)`. `ranges`,
`audit::check_paths`, `samples::instrument_samples`, `samples::folder_samples` and `info::describe` return
reports only. Every report implements `serde::Serialize`.

Differences from the Python tools (`tools/sfz_*.py`):

- **tighten** follows includes, and removes release regions wherever their `trigger` is set (Python dropped
  whole `<group>` blocks).
- **retune** measures the median of up to ten harmonics. Python took the strongest of the first three,
  usually the fundamental. On low plucked notes the instrument's body pulls the fundamental 25-50 cents
  off, so Python's corrections put low pizzicato notes out of tune. Noise samples (releases, pedal) are
  left alone instead of being tuned to whatever peak they have. It measures against the effective
  `pitch_keycenter`, or the sample's root key when `pitch_keycenter=sample`. By default the correction
  replaces the existing `tune`; `keep_existing_tune = true` adds to it, as Python did.
- **even** cuts all sound before each measurement. The Python tool let earlier notes ring into later
  measurements; with a 30 s release that made some keys read up to 10.9 dB too loud.
  `reset_between_notes = false` gives the old behaviour exactly. The layer comes from the velocity ranges,
  not from the text `xfin_lovel`.

## Instrument description

```text
violins.sfz: 131 regions, 131 samples (44100 Hz PCM_16 stereo WAV, 94.8 MB, 0.4-15.2 s)
keys: G3-D6 (55-86), 11 key centres
keyswitches 120-127 (default 120): 120 sus G3-D6, 121 trem G3-D6, 122 spic G3-D6, 123 pizz G3-D6
velocity: 2 layers, changing at 63; CC1 crossfades layers
round robin: 2 sequential, 2 random; release samples: none
controllers: CC1 modulation wheel (default 64): crossfade, cutoff, volume
loops: none
```

Keyswitch names are looked up in this order:

1. `sw_label`;
2. `label_keyN`;
3. a comment next to `sw_last` or above its header;
4. a list in a comment, such as `// keyswitches: 120=sus, 121=trem`.

Controller names come from `label_ccN`, or else from the standard MIDI name.

## Tests and verification

```sh
export CARGO_TARGET_DIR=D:/baton-build/flux-sfz
cargo test -p flux-sfz                                   # unit tests, tests/even.rs (skipped without sfizz), doc tests
cargo test -p flux-sfz --test corpus -- --ignored        # every SFZ of the repository
cargo clippy -p flux-sfz --all-targets -- -D warnings
```

The examples (`sfz_sets`, `sfz_equiv`, `sfz_tools`, `sfz_dump`, `pitch_check`) drive the comparisons with
the Python code and with sfizz. Those comparisons, and their outputs, are in `build/flux-sfz_check/`.

## Dependencies

All dependencies use permissive licences:

| crate | licence | used for |
|---|---|---|
| claxon | Apache-2.0 | FLAC |
| rustfft | MIT/Apache-2.0 | pitch |
| serde, serde_json | MIT/Apache-2.0 | reports |
| sha2 | MIT/Apache-2.0 | manifests |
| thiserror | MIT/Apache-2.0 | errors |
| toml | MIT/Apache-2.0 | sets |

`flux-engine` (a path dependency) provides the sfizz binding that `even` renders through.
