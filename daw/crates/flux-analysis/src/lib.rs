//! Measurements on Flux's audio: what an agent needs to judge a render it cannot hear.
//!
//! Every analysis takes an [`AudioRef`], a borrowed planar stereo view, so it runs in place on a render
//! ([`AudioRef::master`] and [`AudioRef::layers`] of a [`flux_engine::RenderOutput`]) or on a decoded file
//! ([`Audio::read`] for WAV and FLAC). Results are plain structs that serialise to JSON, with a stable `code` or
//! `area` on every finding and a message that says what to change.
//!
//! | Module | What it answers | Replaces |
//! |---|---|---|
//! | [`loudness`] | Does every track of the game sit at the target (-15 LUFS, -1 dBTP, under 20 MB), loop seamlessly, and do its layers sum to the master? Which tracks will jump out next to the others? | `verify.py` loudness and seam checks |
//! | [`reference`](mod@reference) | How does a mix differ from a reference track at equal loudness (balance, dynamics, transients, width, low end, mono), as ranked suggestions, whole and per section? | — |
//! | [`master`] | Is a master ready to ship, and what should change? Loudness (LUFS, LRA, true peak, PLR, PSR), tonal balance against a target curve or a reference, stereo image and phase, dynamics, layers and the loop seam, as ranked issues with moves for Flux's master and a 0-100 score. | — |
//! | [`matcheq`] | The EQ that moves a mix's tonal balance toward a reference's: a few parametric bands fitted with the engine's own biquad responses. | — |
//! | [`mix`] | Which stems mask each other where, which are buried, which sections are muddy or harsh and which stems cause it, does reverb smear the attacks, does the mix survive mono? And which mixer change on which track fixes each. | — |
//! | [`profile`] | The measurements behind both: loudness, LRA, PLR, onset rate and transient clarity, 1/3-octave balance, per-octave width, tone and mono compatibility. | — |
//! | [`spectro`] | Log-frequency spectrogram PNGs, one band per signal (pixel-identical to the Python tool). | `tools/spectro.py` |
//! | [`stems`] | Per-section RMS of each stem: the balance table. | `tools/stem_balance.py` |
//! | [`timefx`] | Time stop envelope (level and spectral centroid) and the played tempo per block of bars. | `tools/time_fx_check.py` |
//! | [`card`] | Instrument report cards: render an SFZ through sfizz and measure range, levels, tuning, dynamics and articulations. | `tools/instrument_card.py` |
//! | [`meta`] | Sections, tempo maps, Flux's layered-export manifest and the pipeline's song metadata. | — |
//!
//! [`spectral`] and [`num`] hold the shared signal processing (STFTs, stereo spectra, K-weighting, 1/3-octave
//! bands, numpy-compatible numerics); [`json`] writes cards the way the Python tool did.
//!
//! # Example
//!
//! ```no_run
//! use flux_analysis::loudness::{GameInput, GameOptions, check_game_audio};
//! use flux_analysis::{Audio, AudioRef};
//! # fn main() -> anyhow::Result<()> {
//! // a render in memory and a file on disk, checked as one game
//! # let render: flux_engine::RenderOutput = unimplemented!();
//! let file = Audio::read(std::path::Path::new("output/far_roads/far_roads.flac"))?;
//! let tracks = vec![
//!     GameInput::from_render("capital_city", &render),
//!     GameInput { name: "far_roads".into(), audio: file.view(), looped: true, bar_starts: vec![], layers: vec![] },
//! ];
//! let report = check_game_audio(tracks, &GameOptions::default())?;
//! for t in &report.tracks {
//!     for f in &t.flags {
//!         println!("{}: [{}] {}", t.name, f.code, f.message);
//!     }
//! }
//! # let _ = AudioRef::master(&render);
//! # Ok(())
//! # }
//! ```
//!
//! The `flux-analyze` binary runs the same analyses on files from the command line (`flux-analyze --help`),
//! and `flux-card` writes instrument cards.

pub mod audio;
pub mod card;
pub mod json;
pub mod loudness;
pub mod master;
pub mod matcheq;
pub mod meta;
pub mod mix;
pub mod num;
pub mod profile;
pub mod reference;
pub mod spectral;
pub mod spectro;
pub mod stems;
pub mod timefx;

pub use audio::{Audio, AudioRef};
