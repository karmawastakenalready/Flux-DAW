//! Mastering report: what an agent needs to judge a master it cannot hear, and the moves Flux's master can make to
//! fix what it finds, so it can render, check, apply the moves and check again until nothing is left.
//!
//! [`check_master`] measures the rendered master, and optionally its layer stems and a reference track:
//!
//! - **Loudness**: integrated, short-term and momentary maxima, loudness range (LRA), true peak, peak-to-loudness
//!   ratio (PLR), peak-to-short-term-loudness ratio (PSR: every 100 ms, the true peak of the last 3 s over their
//!   short-term loudness, so the densest passage shows), crest factor and the distance to the target. A loop is
//!   measured as it plays repeated (see [`crate::loudness::loudness_stats`]) and its seam is checked.
//! - **Technical**: a DC offset, rumble under 20 Hz, and a top end cut off short (a low-bitrate or resampled source).
//! - **Tonal balance**: the loudness-normalised 1/3-octave spectrum (25 Hz to 16 kHz) against a target curve with a
//!   tolerance corridor, per band and over named regions (sub, boom, warmth, mud, boxiness, honk, harshness,
//!   sibilance, air); how much each region comes and goes, and how wide it is; bursts of sibilance, which a static
//!   EQ cannot fix; and the EQ that brings the regions outside the corridor back into it.
//! - **Stereo**: correlation, side over mid and width in four bands against the style's masters or the reference,
//!   the low end's width, what a mono fold-down loses, and phase trouble in each band.
//! - **Dynamics**: PLR and LRA verdicts, squashed transients and the loudness of each section.
//! - **Layers**: each layer stem's loudness and true peak, what each adds stacked on the ones before it, and
//!   whether they sum to the master.
//!
//! Every finding is a [`MasterIssue`] with a stable `code`, a severity and, where Flux's master has a control for
//! it, a [`MasterMove`]. Moves are deliberately small (EQ at most 3 dB, Q 0.7 to 1.4, shelves for broad tilts) so
//! that applying them and checking again converges instead of overshooting, and nothing inside the tolerance gets
//! a move. The tonal moves come from an EQ fitted to the whole correction (at most four bands); a region it leaves
//! out gets one band across it. A region that is too loud only some of the time gets a dynamic EQ band instead, one
//! too loud only in the sides a cut on the side signal, and missing air above a cut-off top the exciter. A region
//! more than 6 dB past its tolerance is more than the master should fix: its message sends the agent to
//! `mix_check`, whose `regions` name the tracks playing there. Between them the moves reach every stage of the
//! master: its EQ (stereo, mid or side bands, a high-pass), dynamic EQ, de-esser, exciter, imager, mono bass, width,
//! loudness and limiter.
//!
//! # Target curves
//!
//! A target is the level of each 1/3-octave band against pink noise's (which has the same power in every band), up
//! to an overall offset, with a tolerance for each band and each region. Both were measured with [`tonal_profile`]
//! on 295 released, professionally mastered tracks under CC BY 4.0, downloaded from the artists' own sites (Kevin
//! MacLeod, Scott Buckley, Silverman Sound, TeknoAXE, Audionautix, Josh Woodward, PeriTune and Nihilore): 72 to 77 per
//! style, by 6 to 8 artists each, none more than a fifth of a style. Each artist counts once per style, so one who
//! released many tracks doesn't set the balance: a style's target is each band's median over its tracks, weighting
//! each track by one over its artist's count. Its spread is the (weighted) standard deviation of each track's
//! deviation from the median of the other artists' tracks, computed the way the report computes deviations, so it
//! says how far a master by an artist the corpus hasn't heard sits from the target. The tolerance is 2.5 spreads, at
//! least 2.5 dB for a band and 2 dB for a region. Only these numbers are part of Flux, not the tracks.
//!
//! | Style | Tracks (artists) | Its balance |
//! |---|---|---|
//! | `balanced` (default) | 73 (7) | bass peaking around 100 Hz; above 1 kHz the highs fall about 3 dB/octave faster than pink |
//! | `cinematic` | 73 (6) | a fuller body from 300 to 800 Hz and darker highs, falling about 5.5 dB/octave faster than pink above 1 kHz |
//! | `electronic` | 72 (6) | a deep sub peaking at 50-63 Hz and the brightest top, falling about 2 dB/octave faster than pink above 1 kHz |
//! | `acoustic` | 77 (8) | little under 80 Hz, a full body from 100 to 500 Hz, and highs falling about 6.5 dB/octave faster than pink |
//!
//! The spreads are widest where masters differ most: 6 to 12 dB in the deep sub and the air, 2.5 to 3 dB through the
//! mids (acoustic recordings, from a solo piano to a guitar duo, differ most: 4 to 5 dB through the mids and about
//! 10 above 5 kHz). Checked artist by artist against tolerances measured without that artist, about one master in six
//! has a region outside, and one in twenty-five has one a spread further out, which makes it a problem rather than a
//! warning. Masters go further below the target than above it from 5 kHz up (dark masters are common, bright ones
//! rare), but a tolerance narrower above didn't catch more out-of-balance masters for the same false alarms, so the
//! tolerance is the same on both sides. With a reference track the target is the reference's own 1/3-octave spectrum
//! at equal loudness, with a tighter, fixed tolerance: 2.5 dB from 100 Hz to 5 kHz, widening to 4.5 dB at 25 Hz and
//! 16 kHz, since the point of a reference is to come close to it (a problem 4 dB past it). Deviations are taken after
//! removing their median (the overall level is the loudness stage's job) and smoothed over neighbouring bands.
//! Without a reference the report also measures the master against every style's target from 100 Hz up and names the
//! closest (`closest_style`). When that is at least 1 dB RMS closer than the style asked for, an `info` issue says so,
//! since a genre's balance is not a fault to correct. The low end is left out of that match because it varies most
//! within a style, and a missing low end is a fault, not a style. Even so the styles overlap: balanced and electronic
//! are 1.8 dB RMS apart from 100 Hz up, and a corpus track's own style is the closest for about three in five.
//!
//! # The other checks
//!
//! The stereo, dynamic and technical limits come from the same corpus, grown to 297 tracks, and were checked against
//! it: 140 of its masters raise no warning at all (leaving out loudness and peaks, since they weren't made for this
//! target), and 84 raise one.
//!
//! - **Phase**: each band's left/right correlation has a warning and a problem limit, lower the higher the band
//!   (reverb and wide synths decorrelate the highs): 0.1 and -0.1 under 150 Hz, down to -0.35 and -0.6 above 5 kHz.
//!   One master in twelve goes under a warning limit and one in thirty under a problem's, most of those recordings
//!   with a channel's polarity flipped. The low band's move is mono bass; the others' an imager band narrowed to a
//!   correlation of 0.2.
//! - **Width**: the low end's side over mid that 85 and 95 % of each style's masters stay under is a note and a
//!   warning (-13 and -6 dB for balanced masters, -2 and -1 for cinematic ones), never closer than -1 dB, where the
//!   bass loses over 2 dB in mono. Each band's side over mid is held to the style's median within one spread (at
//!   least 2 dB), or to the reference's within 3 dB; past it a band gets an imager move. A whole master whose
//!   correlation falls under 0 or whose mono fold-down loses over 3.5 LU is too wide: one in twenty of the corpus.
//! - **Coming and going**: a region whose loud moments stand further over its usual level than three in four of
//!   the corpus' masters' do gets a dynamic EQ band rather than a cut. Measured on 99 of them, a band from 4 dB under
//!   the region's loud level, ratio 3, brings the region's average down by a third of its range.
//! - **Sibilance**: bursts whose 99th percentile stands 12 dB over their median and 15 dB over the target's share of
//!   5 to 10 kHz are noted (one master in fourteen), 17 dB over it a warning (one in thirty). The de-esser a move
//!   adds catches only the loudest moments (see [`DEESS_THRESHOLD_DB`]), since on a whole mix it dulls the air about
//!   twice as much as it tames the bursts.
//! - **Technical**: a DC offset over -40 dBFS (the corpus' largest is -44), power under 20 Hz less than 12 dB under
//!   the rest (the corpus stays under -16, most under -40), and a top end that drops 30 dB within a third of an
//!   octave: a warning under 15 kHz, a note under 17 kHz. Seven corpus masters stop between 15.6 and 19.8 kHz; a
//!   dark master that rolls off steeply is not cut off.
//!
//! # Score
//!
//! `score` is 100 less the points each finding costs, clamped to 0..100; every lost point has an issue saying why.
//! A master on target with nothing flagged scores 100; an `info` costs nothing.
//!
//! | Finding | Points |
//! |---|---|
//! | integrated loudness off target | 6 per LU beyond 0.5 LU, at most 25 |
//! | true peak over the ceiling | 10, plus 10 per dB over, at most 25 |
//! | samples at full scale | 10 |
//! | PLR under 8 dB | 4 per dB under, at most 20 |
//! | a loop's LRA over 10 LU | 2 per LU over, at most 10 |
//! | tonal regions outside the tolerance | 0.6 per dB beyond the tolerance, summed over their bands (each at most 6 dB), at most 30 |
//! | phase trouble | 15 (problem) or 6 (warning) |
//! | wide low end | 6 (warning) or 2 (info) |
//! | too wide | 5 |
//! | a band wider than the target's | 2 |
//! | a DC offset, rumble under 20 Hz, a top end cut off under 15 kHz | 3 each |
//! | sibilance bursts | 5 (warning) |
//! | squashed transients | 5 |
//! | a loud passage, a dense passage | 3 each |
//! | layers (unusable 3, silent 3, over the ceiling 3 or 5, a partial stack over it 3, not summing 10) | at most 20 |
//! | loop seam: jump, click, level step | 10, 5, 3 |

use crate::audio::AudioRef;
use crate::loudness::{LoudnessStats, SeamReport, loop_seam, loudness_stats};
use crate::matcheq::{EqBand, EqShape, FitOptions, LongTermSpectrum, fit_eq, log_grid};
use crate::num::{hann_periodic, median, percentile, rfft};
use crate::profile::{LOW_END_HZ, LOW_END_MATTERS_DB, Profile, SPECTRUM_N, pink_level_db, profile};
use crate::spectral::{StereoSpectrum, third_octave_centres, third_octave_edges, third_octave_label};
use anyhow::{Context, Result, anyhow, ensure};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Which family of commercial masters the tonal target follows (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TonalStyle {
    /// Band-based game and pop music: drums, bass, guitars or keys with other instruments.
    #[default]
    Balanced,
    /// Orchestral and trailer music: a fuller low-mid body and darker highs than balanced.
    Cinematic,
    /// Synth and beat-driven music: a deep sub and the brightest top.
    Electronic,
    /// Acoustic instruments and small ensembles: little deep bass, a full body from 100 to 500 Hz.
    Acoustic,
}

impl TonalStyle {
    pub const ALL: [TonalStyle; 4] = [TonalStyle::Balanced, TonalStyle::Cinematic, TonalStyle::Electronic, TonalStyle::Acoustic];

    pub fn name(self) -> &'static str {
        match self {
            TonalStyle::Balanced => "balanced",
            TonalStyle::Cinematic => "cinematic",
            TonalStyle::Electronic => "electronic",
            TonalStyle::Acoustic => "acoustic",
        }
    }

    /// The music it is for, in words.
    pub fn music(self) -> &'static str {
        match self {
            TonalStyle::Balanced => "band-based game or pop music",
            TonalStyle::Cinematic => "orchestral or trailer music",
            TonalStyle::Electronic => "synth or beat-driven music",
            TonalStyle::Acoustic => "acoustic instruments or a small ensemble",
        }
    }

    /// What the style's corpus measured (see the module docs).
    fn measured(self) -> &'static Measured {
        match self {
            TonalStyle::Balanced => &BALANCED,
            TonalStyle::Cinematic => &CINEMATIC,
            TonalStyle::Electronic => &ELECTRONIC,
            TonalStyle::Acoustic => &ACOUSTIC,
        }
    }
}

impl FromStr for TonalStyle {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<TonalStyle> {
        match s.trim().to_ascii_lowercase().as_str() {
            "balanced" => Ok(TonalStyle::Balanced),
            "cinematic" => Ok(TonalStyle::Cinematic),
            "electronic" => Ok(TonalStyle::Electronic),
            "acoustic" => Ok(TonalStyle::Acoustic),
            _ => Err(anyhow!("unknown tonal style '{s}': use balanced, cinematic, electronic or acoustic")),
        }
    }
}

/// What a style's corpus of professional masters measured, in dB: per 1/3-octave band from 25 Hz to 16 kHz (band 0
/// is 25 Hz) and per tonal region, in [`REGIONS`]' order.
struct Measured {
    /// Each band's median level against pink noise, each artist counting once, the median over the bands taken off
    /// (what [`tonal_profile`] returns).
    median: [f64; 29],
    /// How far a master's band sits from the median: one standard deviation of each master's smoothed deviation
    /// from the median of the other artists' masters, smoothed over neighbouring bands.
    band_spread: [f64; 29],
    /// The same for each region's mean deviation, as [`check_master`] computes it.
    region_spread: [f64; 9],
    /// Side over mid power in the low-mid, mid and high stereo bands ([`STEREO_BANDS`] after the low one): the
    /// artist-weighted median and spread, dB.
    width_median: [f64; 3],
    width_spread: [f64; 3],
    /// The same in each tonal region.
    side_median: [f64; 9],
    side_spread: [f64; 9],
    /// The low end's side over mid (under [`LOW_END_HZ`]) that 85 and 95 % of the masters stay under, dB: over the
    /// first the low end is noted as wide, over the second it is a warning. At most [`LOW_SIDE_CAP_DB`].
    low_side_db: [f64; 2],
}

const BALANCED: Measured = Measured {
    median: [
        -18.6, -12.2, -2.9, 3.6, 7.2, 7.3, 8.3, 6.1, 5.1, 6.1, 3.5, 4.5, 3.5, 2.5, 2.1, 1.7, 0.0, 0.3, -0.8, -1.3, -1.4,
        -3.8, -4.4, -6.3, -7.7, -8.8, -11.0, -12.3, -16.4,
    ],
    band_spread: [
        7.8, 6.9, 6.0, 5.3, 4.6, 3.8, 3.2, 2.7, 2.7, 2.9, 2.9, 2.9, 3.1, 3.0, 3.0, 3.1, 3.2, 3.0, 2.7, 2.7, 2.9, 2.9, 3.0,
        3.2, 3.7, 4.3, 5.1, 6.0, 6.8,
    ],
    region_spread: [7.4, 4.4, 2.5, 2.7, 2.9, 2.8, 2.7, 3.6, 6.0],
    width_median: [-5.3, -4.1, -4.0],
    width_spread: [3.7, 3.2, 4.0],
    side_median: [-19.2, -17.5, -11.1, -5.3, -5.0, -4.2, -3.3, -4.2, -3.5],
    side_spread: [9.7, 9.9, 5.4, 3.8, 3.2, 3.2, 4.1, 4.1, 4.2],
    low_side_db: [-13.0, -6.0],
};

const CINEMATIC: Measured = Measured {
    median: [
        -19.7, -9.4, -5.4, -2.3, 3.2, 4.6, 5.7, 3.6, 3.8, 3.5, 3.1, 4.8, 5.4, 4.7, 4.8, 3.9, 2.3, 2.4, 0.0, -1.1, -2.3,
        -4.0, -6.8, -9.4, -11.0, -14.5, -17.6, -19.8, -24.3,
    ],
    band_spread: [
        9.9, 10.0, 8.9, 6.9, 5.1, 4.0, 3.5, 3.4, 3.3, 3.3, 3.5, 3.6, 3.3, 2.8, 2.4, 2.2, 2.2, 2.3, 2.4, 2.5, 2.6, 2.8, 3.0,
        3.5, 4.4, 5.5, 6.2, 6.4, 6.5,
    ],
    region_spread: [10.4, 5.3, 3.1, 3.2, 2.3, 2.2, 2.5, 4.2, 7.1],
    width_median: [-1.7, -0.9, -1.5],
    width_spread: [1.8, 1.8, 3.0],
    side_median: [-9.7, -5.2, -2.8, -1.7, -1.1, -1.2, -1.1, -1.4, -1.8],
    side_spread: [8.3, 6.6, 2.5, 2.0, 1.8, 1.8, 2.3, 2.9, 3.7],
    low_side_db: [-2.0, -1.0],
};

const ELECTRONIC: Measured = Measured {
    median: [
        -15.6, -4.0, 2.2, 7.5, 8.6, 6.0, 5.3, 3.7, 3.4, 3.0, 1.4, 3.0, 2.3, 1.7, 2.0, 1.1, 0.0, 0.0, -0.9, -2.0, -2.5,
        -3.8, -4.2, -4.7, -5.3, -6.2, -7.5, -9.9, -12.9,
    ],
    band_spread: [
        7.1, 6.7, 5.5, 3.9, 3.0, 2.7, 2.7, 2.7, 2.8, 2.9, 3.0, 3.2, 3.2, 3.1, 2.8, 2.6, 2.5, 2.4, 2.3, 2.4, 2.8, 3.4, 4.0,
        4.5, 4.6, 4.5, 5.0, 5.8, 6.2,
    ],
    region_spread: [7.0, 2.9, 2.5, 2.9, 2.7, 2.3, 3.0, 4.8, 5.7],
    width_median: [-4.7, -3.2, -5.3],
    width_spread: [4.3, 4.6, 5.8],
    side_median: [-20.4, -20.2, -9.8, -5.0, -3.3, -3.0, -4.7, -5.4, -5.9],
    side_spread: [12.8, 11.4, 6.7, 4.4, 4.0, 4.4, 5.6, 6.1, 5.2],
    low_side_db: [-8.5, -4.0],
};

const ACOUSTIC: Measured = Measured {
    median: [
        -24.8, -19.9, -16.2, -13.1, -8.2, 1.3, 8.0, 8.8, 9.1, 10.9, 11.0, 11.4, 11.4, 9.3, 9.0, 7.9, 5.6, 3.8, 1.2, 0.0,
        -3.8, -4.9, -7.9, -10.5, -13.0, -13.9, -15.7, -17.3, -22.2,
    ],
    band_spread: [
        10.6, 11.4, 12.4, 12.7, 11.4, 8.8, 7.6, 8.7, 9.3, 7.8, 5.7, 4.5, 4.6, 5.0, 5.1, 4.7, 4.4, 4.2, 3.9, 4.0, 4.7, 6.0,
        7.5, 8.8, 9.6, 10.1, 9.9, 9.4, 9.6,
    ],
    region_spread: [11.2, 11.1, 8.9, 4.7, 4.9, 3.9, 5.1, 9.9, 10.7],
    width_median: [-3.4, -2.5, -1.1],
    width_spread: [6.0, 4.7, 4.4],
    side_median: [-9.0, -10.1, -6.7, -3.8, -2.5, -2.3, -2.1, -1.3, -0.5],
    side_spread: [7.4, 7.9, 6.1, 6.0, 5.4, 4.7, 4.4, 4.7, 4.8],
    low_side_db: [-3.0, -1.0],
};

/// How many of the corpus' standard deviations a band or region may sit from the median before it is outside.
const TOLERANCE_SPREADS: f64 = 2.5;

/// A per-band table's value at `f` Hz: linear in log frequency between the band centres, held at the ends.
fn at_hz(table: &[f64; 29], f: f64) -> f64 {
    // the centres are 10^(i/10) Hz for i = 14..=42
    let x = (10.0 * f.max(1.0).log10() - 14.0).clamp(0.0, 28.0);
    let i = (x as usize).min(27);
    table[i] + (table[i + 1] - table[i]) * (x - i as f64)
}

/// The target curve of `style` at `f` Hz: a 1/3-octave band's level against pink noise's, dB, up to an overall
/// offset (see the module docs).
pub fn target_db(style: TonalStyle, f: f64) -> f64 {
    at_hz(&style.measured().median, f)
}

/// How far a 1/3-octave band centred on `f` Hz may sit from `style`'s target, dB: 2.5 of the corpus' spreads
/// there, at least 2.5.
pub fn tolerance_db(style: TonalStyle, f: f64) -> f64 {
    (TOLERANCE_SPREADS * at_hz(&style.measured().band_spread, f)).max(2.5)
}

/// How far the mean deviation of the region `REGIONS[ri]` may sit from `style`'s target, dB: 2.5 of the corpus'
/// spreads there, at least 2.
fn region_tolerance_db(style: TonalStyle, ri: usize) -> f64 {
    (TOLERANCE_SPREADS * style.measured().region_spread[ri]).max(2.0)
}

/// How far a 1/3-octave band centred on `f` Hz may sit from a reference track, dB: 2.5 from 100 Hz to 5 kHz,
/// widening to 4.5 at 25 Hz and 16 kHz. Tighter than a style's: the point of a reference is to come close to it.
fn reference_tolerance_db(f: f64) -> f64 {
    let x = if f < 100.0 {
        (100.0 / f).log2() / 2.0
    } else if f > 5000.0 {
        (f / 5000.0).log2() / 3.2f64.log2()
    } else {
        0.0
    };
    2.5 + 2.0 * x.clamp(0.0, 1.0)
}

/// Options of [`check_master`].
#[derive(Clone, Debug)]
pub struct MasterOptions<'a> {
    /// Integrated loudness the master should reach, LUFS.
    pub target_lufs: f64,
    /// Highest true peak allowed, dBTP.
    pub ceiling_dbtp: f64,
    /// The master loops in the game: it is measured as it plays repeated, its seam is checked, and its loudness
    /// range is held to a loop's narrower standard.
    pub looped: bool,
    /// Sample positions of the bar lines, which the seam is compared with (empty: the track's own attacks).
    pub bar_starts: Vec<usize>,
    /// Layer stems that should sum to the master, in stacking order (same sample rate and length as the master).
    pub layers: Vec<(String, AudioRef<'a>)>,
    /// A reference track: its own spectrum at equal loudness becomes the tonal target.
    pub reference: Option<AudioRef<'a>>,
    /// The tonal target when there is no reference.
    pub style: TonalStyle,
    /// Named sections as sample ranges `(name, start, end)`, for the loudness of each.
    pub sections: Vec<(String, usize, usize)>,
}

impl Default for MasterOptions<'_> {
    fn default() -> Self {
        MasterOptions {
            target_lufs: -15.0,
            ceiling_dbtp: -1.0,
            looped: false,
            bar_starts: vec![],
            layers: vec![],
            reference: None,
            style: TonalStyle::Balanced,
            sections: vec![],
        }
    }
}

/// Which part of the stereo signal an EQ move works on (as the `channel` of Flux's EQ bands).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// Both channels alike.
    #[default]
    Stereo,
    /// The mid signal: the centre of the image.
    Mid,
    /// The side signal: the edges of the image.
    Side,
}

impl Part {
    pub fn is_stereo(&self) -> bool {
        *self == Part::Stereo
    }
}

/// The curve of an exciter band (as the `mode` of Flux's exciter bands).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Saturation {
    /// Odd harmonics: clear and bright.
    Tape,
    /// Even and odd harmonics: warmer.
    Tube,
}

/// A change to Flux's master that addresses an issue. Each is one setting, small enough that applying it and
/// checking again converges.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MasterMove {
    /// Add this band to the master EQ (gain at most 3 dB, 4.5 on the side signal), on `part` of the stereo signal
    /// (left out: both channels).
    Eq {
        shape: EqShape,
        hz: f64,
        db: f64,
        q: f64,
        #[serde(default, skip_serializing_if = "Part::is_stereo")]
        part: Part,
    },
    /// Add a high-pass at `hz` to the master EQ, falling `slope` dB per octave (or raise the one there to `hz`).
    HighPass { hz: f64, slope: u32 },
    /// Make the master mono below `hz` (the master's `mono_below_hz`).
    MonoBelow { hz: f64 },
    /// Scale the master's stereo width by this factor (0.8: 20 % narrower than it is now). The report cannot see
    /// the current setting, so the move is relative: multiply the master's `width` by it.
    Width { width: f64 },
    /// Turn on the master's de-esser, or deepen it: the band above `hz` turned down by at most `range_db` (negative)
    /// while it's louder than its loud level (a new one starts at [`DEESS_THRESHOLD_DB`]). A de-esser on the tracks
    /// that carry the sibilance (mix_check's regions name them) is better still.
    DeEss { hz: f64, range_db: f64 },
    /// Add a band to the master's dynamic EQ: `shape` at `hz` (Q `q`), turned down by at most `range_db` (negative)
    /// while the range is loud. For a range that is too loud only some of the time, which a static cut would thin
    /// out the rest of the time.
    DynEq { shape: EqShape, hz: f64, q: f64, range_db: f64 },
    /// Saturate `hz_lo..hz_hi` in the master's exciter, blending in at least `mix` of it on `mode`'s curve: the
    /// harmonics it adds above a cut-off top end bring back some of the air an EQ can't.
    Exciter { hz_lo: f64, hz_hi: f64, mix: f64, mode: Saturation },
    /// Scale the width of `hz_lo..hz_hi` in the master's imager by `width`. Relative, as `Width` is: 0.8 makes the
    /// band 20 % narrower than it is now.
    Imager { hz_lo: f64, hz_hi: f64, width: f64 },
    /// Set the master's loudness target, LUFS.
    Loudness { target_lufs: f64 },
    /// Change the final limiter: `release_ms` (its recovery) and `clip_db` (how much the soft-clip stage takes off
    /// the sharpest peaks, positive dB); `None` leaves a setting as it is.
    Limiter { release_ms: Option<f64>, clip_db: Option<f64> },
}

/// One finding, ranked: problems first, then warnings, then notes, the costliest first within each.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MasterIssue {
    /// Stable code agents can match on (`loudness_target`, `muddy`, `phase`...).
    pub code: String,
    /// "problem" (breaks the delivery spec or is clearly audible), "warn" (likely audible) or "info".
    pub severity: &'static str,
    /// What was found, why it matters and what to do.
    pub message: String,
    /// The master change that addresses it, if the master has a control for it.
    pub suggestion: Option<MasterMove>,
}

/// Loudness and peaks.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MasterLoudness {
    pub integrated_lufs: Option<f64>,
    /// Loudest 3 s, LUFS.
    pub short_term_max_lufs: Option<f64>,
    /// Loudest 400 ms, LUFS.
    pub momentary_max_lufs: Option<f64>,
    pub lra_lu: Option<f64>,
    pub true_peak_dbtp: f64,
    pub sample_peak_dbfs: f64,
    /// True peak over integrated loudness, dB.
    pub plr_db: Option<f64>,
    /// Smallest peak-to-short-term-loudness ratio over the loud passages, dB: the densest 3 s.
    pub psr_min_db: Option<f64>,
    /// Middle of that 3 s window, seconds.
    pub psr_min_at_s: Option<f64>,
    pub psr_median_db: Option<f64>,
    /// Sample peak over RMS, dB.
    pub crest_db: f64,
    /// Samples at digital full scale.
    pub clipped_samples: usize,
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
    /// Integrated loudness minus the target, LU (positive: too loud).
    pub from_target_lu: Option<f64>,
    /// The ceiling minus the true peak, dB (negative: over the ceiling).
    pub headroom_db: f64,
}

/// One 1/3-octave band against the target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TonalBand {
    pub band: String,
    pub centre_hz: f64,
    /// Loudness-normalised level, dB (the profile's band level).
    pub level_db: Option<f64>,
    /// The target's level for the band, shifted by the overall offset so the two compare directly, dB.
    pub target_db: Option<f64>,
    /// Level minus target with the overall offset removed, smoothed over the neighbouring bands, dB.
    pub deviation_db: Option<f64>,
    pub tolerance_db: f64,
    /// `|deviation_db| > tolerance_db`.
    pub outside: bool,
}

/// A named frequency region against the target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TonalRegion {
    /// sub, boom, warmth, mud, boxy, honk, harsh, sibilance or air.
    pub region: &'static str,
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// Mean deviation of its bands, dB.
    pub deviation_db: Option<f64>,
    /// Mean tolerance of its bands, dB.
    pub tolerance_db: f64,
    /// "ok", "high" or "low".
    pub verdict: &'static str,
    /// The issue code when outside the tolerance (`muddy`, `thin`, `dull`...).
    pub code: Option<&'static str>,
    /// 90th minus 50th percentile of the region's share of each 170 ms frame, dB: how much it comes and goes.
    pub variability_db: Option<f64>,
    /// Side over mid power in the region, dB: how wide it is.
    pub side_mid_db: Option<f64>,
}

/// Short bursts of 5-10 kHz (sibilants, splashes), measured in 21 ms frames of the mid signal.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SibilanceBursts {
    /// 99th percentile of the band's share of each frame, dB: the loudest bursts.
    pub spike_db: f64,
    /// `spike_db` minus the median share: how far the bursts stand out of the rest, dB.
    pub burst_db: f64,
    /// The band's share of a master on the target, dB.
    pub expected_db: f64,
    /// Bursts stand 9 dB out of the rest and 8 dB over the expected share: a de-esser's job, not an EQ's.
    pub bursty: bool,
    /// Centre of the 4-12 kHz content of the loudest bursts, Hz.
    pub centre_hz: Option<f64>,
}

/// The tonal balance against the target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TonalBalance {
    /// "balanced", "cinematic", "electronic", "acoustic" or "reference".
    pub target: &'static str,
    /// Level minus target, median over the bands: the overall offset removed before comparing, dB.
    pub offset_db: f64,
    /// RMS of the band deviations, dB.
    pub deviation_rms_db: f64,
    pub bands: Vec<TonalBand>,
    pub regions: Vec<TonalRegion>,
    pub sibilance: Option<SibilanceBursts>,
    /// The master EQ that brings the regions outside the tolerance back to within half of it (at most 4 bands of
    /// at most 3 dB): the issues' EQ suggestions are taken from it.
    pub correction: Vec<EqBand>,
    /// How far the master is from each style's target (empty against a reference), and the closest.
    pub styles: Vec<StyleFit>,
    pub closest_style: Option<TonalStyle>,
}

/// How far a master's balance is from one style's target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StyleFit {
    pub style: TonalStyle,
    /// RMS of the band deviations, dB (as `TonalBalance::deviation_rms_db`).
    pub deviation_rms_db: f64,
}

/// One band's stereo image.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StereoRegion {
    /// low (under 150 Hz), low_mid, mid or high (over 5 kHz).
    pub band: &'static str,
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// Share of the loudness-normalised power, dB.
    pub level_db: Option<f64>,
    /// Left/right correlation (1 mono, 0 unrelated, -1 out of phase).
    pub correlation: Option<f64>,
    /// Side over mid power, dB.
    pub side_mid_db: Option<f64>,
    /// Side share `S / (M + S)`: 0 mono, 0.5 unrelated, 1 out of phase.
    pub width: Option<f64>,
}

/// Stereo image and mono compatibility.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MasterStereo {
    pub correlation: Option<f64>,
    /// Loudness of the mono fold-down minus the stereo loudness, LU.
    pub fold_down_lu: Option<f64>,
    /// Share of 400 ms windows with negative correlation.
    pub negative_share: f64,
    /// Side over mid below 120 Hz, dB (under about -20 the low end is mono).
    pub low_end_side_db: Option<f64>,
    /// Power below 120 Hz against the whole, dB.
    pub low_end_db: Option<f64>,
    pub bands: Vec<StereoRegion>,
    /// A band that matters is out of phase enough to cancel in mono.
    pub phase_problem: bool,
}

/// Loudness of one section.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionLoudness {
    pub name: String,
    pub start_s: f64,
    pub end_s: f64,
    pub lufs: Option<f64>,
    pub short_term_max_lufs: Option<f64>,
    pub short_term_median_lufs: Option<f64>,
    /// `lufs` minus the whole master's, LU.
    pub vs_song_lu: Option<f64>,
}

/// Dynamics.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MasterDynamics {
    pub plr_db: Option<f64>,
    /// "crushed" (PLR under 6 dB), "over_limited" (under 8 dB), "ok" or "unknown".
    pub plr_verdict: &'static str,
    pub lra_lu: Option<f64>,
    /// "flat" (under 1.5 LU), "wide" (over 10 LU for a loop, 15 for a one-shot), "ok" or "unknown".
    pub lra_verdict: &'static str,
    /// Detected attacks per second.
    pub onsets_per_s: f64,
    /// Median contrast of the attacks over what precedes them, dB.
    pub transient_contrast_db: Option<f64>,
    /// Median level 100-150 ms after an attack's peak, dB re the peak.
    pub transient_tail_db: Option<f64>,
    /// Attacks barely stand out of a dense, limited master.
    pub squashed_transients: bool,
    pub sections: Vec<SectionLoudness>,
}

/// One layer stem.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayerLevel {
    pub name: String,
    pub lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    /// This layer stacked on the ones before it, LUFS.
    pub stacked_lufs: Option<f64>,
    /// How much louder the stack gets with this layer, LU (`None` for the first, or after silence).
    pub adds_lu: Option<f64>,
    pub stacked_true_peak_dbtp: Option<f64>,
    /// Its own true peak is over the ceiling.
    pub over_ceiling: bool,
    /// Under -40 LUFS.
    pub silent: bool,
    /// Within 0.5 LU of the whole master on its own: the others add little.
    pub dominates: bool,
    /// Why it could not be measured (sample rate or length differ from the master's).
    pub error: Option<String>,
}

/// The layer stems against the master.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MasterLayers {
    pub layers: Vec<LayerLevel>,
    /// Largest `|sum of layers - master|`, dBFS (`None` when a layer could not be used).
    pub sum_error_dbfs: Option<f64>,
    /// The sum is within -60 dBFS of the master.
    pub sums_to_master: Option<bool>,
}

/// Faults a master shouldn't have whatever its style: a DC offset, rumble under hearing, a cut-off top end.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MasterTechnical {
    /// The larger of the two channels' DC offsets (their mean), dBFS (-120 for none).
    pub dc_offset_dbfs: f64,
    /// Power from 5 to 20 Hz against the power above 20 Hz, dB: rumble no speaker plays that still costs headroom.
    pub subsonic_db: Option<f64>,
    /// Where the top end is cut off, Hz: above it everything sits 30 dB under the octave below, as after a low-pass
    /// or a low-bitrate encode. `None` when the spectrum runs on to 20 kHz (or near the Nyquist frequency).
    pub band_limit_hz: Option<f64>,
}

/// The result of [`check_master`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MasterReport {
    /// One line: loudness, peaks, PLR, LRA, the issues by count and code, and the score.
    pub summary: String,
    /// 0 to 100 (see the module docs).
    pub score: u32,
    pub style: TonalStyle,
    pub looped: bool,
    pub duration_s: f64,
    pub sample_rate: u32,
    pub loudness: MasterLoudness,
    pub technical: MasterTechnical,
    pub tonal: Option<TonalBalance>,
    pub stereo: MasterStereo,
    pub dynamics: MasterDynamics,
    pub layers: Option<MasterLayers>,
    pub seam: Option<SeamReport>,
    /// Findings, the most important first.
    pub issues: Vec<MasterIssue>,
}

fn rd(x: f64, d: i32) -> f64 {
    let k = 10f64.powi(d);
    (x * k).round() / k
}

fn r1(x: f64) -> f64 {
    rd(x, 1)
}

fn r2(x: f64) -> f64 {
    rd(x, 2)
}

fn db10(x: f64) -> Option<f64> {
    (x > 1e-30 && x.is_finite()).then(|| 10.0 * x.log10())
}

fn db20(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// "20", "500", "1k", "2.5k".
fn hz_text(f: f64) -> String {
    if f >= 1000.0 { format!("{}k", r1(f / 1000.0)) } else { format!("{f:.0}") }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sev {
    Problem,
    Warn,
    Info,
}

impl Sev {
    fn name(self) -> &'static str {
        match self {
            Sev::Problem => "problem",
            Sev::Warn => "warn",
            Sev::Info => "info",
        }
    }

    /// One step milder.
    fn milder(self) -> Sev {
        match self {
            Sev::Problem => Sev::Warn,
            _ => Sev::Info,
        }
    }
}

/// Findings as they are made: each with the weight that ranks it, and the points the score loses.
#[derive(Default)]
struct Findings {
    items: Vec<(Sev, f64, MasterIssue)>,
    penalty: f64,
}

impl Findings {
    /// Record a finding ranked by `weight` (the score's points are counted by the caller).
    fn add(&mut self, sev: Sev, code: &str, weight: f64, message: String, suggestion: Option<MasterMove>) {
        let message = match &suggestion {
            Some(m) => format!("{message}. Try: {}", describe(m)),
            None => message,
        };
        self.items.push((sev, weight, MasterIssue { code: code.into(), severity: sev.name(), message, suggestion }));
    }

    /// Record a finding that costs the score `points`.
    fn cost(&mut self, sev: Sev, code: &str, points: f64, message: String, suggestion: Option<MasterMove>) {
        self.penalty += points;
        self.add(sev, code, points, message, suggestion);
    }

    fn sorted(mut self) -> Vec<MasterIssue> {
        self.items.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));
        self.items.into_iter().map(|i| i.2).collect()
    }
}

/// A move in words, for the issue's message.
fn describe(m: &MasterMove) -> String {
    let shape_name = |s: EqShape| match s {
        EqShape::Peak => "peak",
        EqShape::LowShelf => "low shelf",
        EqShape::HighShelf => "high shelf",
    };
    match *m {
        MasterMove::Eq { shape, hz, db, q, part } => {
            let on = match part {
                Part::Stereo => "",
                Part::Mid => " on the mid signal",
                Part::Side => " on the side signal",
            };
            format!("a master EQ {} of {db:+.1} dB at {} Hz, Q {q:.2}{on}", shape_name(shape), hz_text(hz))
        }
        MasterMove::HighPass { hz, slope } => format!("a master EQ high-pass at {} Hz, {slope} dB per octave", hz_text(hz)),
        MasterMove::MonoBelow { hz } => format!("make the master mono below {hz:.0} Hz"),
        MasterMove::Width { width } => format!("scale the master's width by {width:.2}"),
        MasterMove::DeEss { hz, range_db } => {
            format!("the master's de-esser on the band above {} Hz, turning it down by up to {:.1} dB while it's loud", hz_text(hz), -range_db)
        }
        MasterMove::DynEq { shape, hz, q, range_db } => format!(
            "a master dynamic EQ {} at {} Hz, Q {q:.2}, turning it down by up to {:.1} dB while it's loud",
            shape_name(shape),
            hz_text(hz),
            -range_db
        ),
        MasterMove::Exciter { hz_lo, hz_hi, mix, mode } => format!(
            "the master's exciter on {}-{} Hz, {} curve, mix {mix:.2}",
            hz_text(hz_lo),
            hz_text(hz_hi),
            if mode == Saturation::Tape { "tape" } else { "tube" }
        ),
        MasterMove::Imager { hz_lo, hz_hi, width } => format!("scale the width of {}-{} Hz in the master's imager by {width:.2}", hz_text(hz_lo), hz_text(hz_hi)),
        MasterMove::Loudness { target_lufs } => format!("set the master's loudness target to {target_lufs:.1} LUFS"),
        MasterMove::Limiter { release_ms, clip_db } => {
            let mut parts = vec![];
            if let Some(c) = clip_db {
                parts.push(format!("let the limiter's clip stage take up to {c:.1} dB"));
            }
            if let Some(r) = release_ms {
                parts.push(format!("set its release to {r:.0} ms"));
            }
            if parts.is_empty() { "leave the limiter as it is".into() } else { parts.join(" and ") }
        }
    }
}

/// Measure a master and say what to change (see the module docs). `a` is the rendered master (at least 0.5 s).
/// About as slow as two passes of the loudness meter over the master plus one per layer; a few seconds for a
/// three-minute song with layers.
pub fn check_master(a: AudioRef, o: &MasterOptions) -> Result<MasterReport> {
    ensure!(a.len() >= a.sr as usize / 2, "the master is {:.2} s long: give at least 0.5 s", a.duration_s());
    ensure!((-40.0..=0.0).contains(&o.target_lufs), "target_lufs is {}: give -40 to 0 LUFS (Roblox: -15)", o.target_lufs);
    ensure!((-20.0..=0.0).contains(&o.ceiling_dbtp), "ceiling_dbtp is {}: give -20 to 0 dBTP (Roblox: -1)", o.ceiling_dbtp);
    if let Some(r) = o.reference {
        ensure!(r.len() >= r.sr as usize / 2, "the reference is {:.2} s long: give at least 0.5 s", r.duration_s());
    }
    for (name, s, e) in &o.sections {
        ensure!(s < e && *s < a.len(), "section '{name}' runs from sample {s} to {e}: it must start before it ends and inside the master ({} samples)", a.len());
    }
    let stats = loudness_stats(a, o.looped).context("loudness")?;
    let Some(lufs) = stats.integrated_lufs else {
        return Ok(silent_report(a, o, &stats));
    };
    let ((prof, series), ((spec, refspec), ((variability, bursts), (layers, seam)))) = rayon::join(
        || rayon::join(|| profile(a), || loudness_series(a, o.looped)),
        || {
            rayon::join(
                || (StereoSpectrum::new(a, SPECTRUM_N), o.reference.map(|r| StereoSpectrum::new(r, SPECTRUM_N))),
                || {
                    rayon::join(
                        || (region_variability(a), sibilance_bursts(a)),
                        || {
                            let layers = (!o.layers.is_empty()).then(|| layer_report(a, &o.layers, o.looped, lufs, o.ceiling_dbtp));
                            (layers, o.looped.then(|| loop_seam(a, &o.bar_starts)).transpose())
                        },
                    )
                },
            )
        },
    );
    let prof = prof.context("profile")?;
    let series = series.context("loudness series")?;
    let refmix = refspec.as_ref().map(LongTermSpectrum::from_stereo).transpose().context("the reference")?;
    let seam = seam.context("loop seam")?;
    let mut f = Findings::default();

    let loudness = loudness_part(&stats, Some(&series), o);
    loudness_issues(&loudness, &mut f);
    let technical = technical(a, &spec);
    technical_issues(&technical, &mut f);
    let tonal = LongTermSpectrum::from_stereo(&spec).ok().map(|mix| {
        let reference = refmix.as_ref().zip(refspec.as_ref());
        tonal(&mix, &spec, reference, o.style, &variability, bursts.as_ref(), technical.band_limit_hz, &mut f)
    });
    let stereo = stereo(&spec, refspec.as_ref(), o.style, &prof, &mut f);
    let dynamics = dynamics(a, o, &stats, &prof, &series, &mut f);
    if let Some(l) = &layers {
        layer_issues(l, lufs, o.ceiling_dbtp, &mut f);
    }
    if let Some(s) = &seam {
        seam_issues(s, &mut f);
    }
    let score = (100.0 - f.penalty).clamp(0.0, 100.0).round() as u32;
    let issues = f.sorted();
    Ok(MasterReport {
        summary: summary(&loudness, &issues, score),
        score,
        style: o.style,
        looped: o.looped,
        duration_s: r2(a.duration_s()),
        sample_rate: a.sr,
        loudness,
        technical,
        tonal,
        stereo,
        dynamics,
        layers,
        seam,
        issues,
    })
}

/// The tonal balance as the target curves see it: each 1/3-octave band's centre (Hz) and its level against pink
/// noise's (dB, the median over the bands taken off; `None` for a band 60 dB under the loudest). What a target curve
/// is measured from.
pub fn tonal_profile(a: AudioRef) -> Result<Vec<(f64, Option<f64>)>> {
    ensure!(a.len() >= a.sr as usize / 2, "the audio is {:.2} s long: give at least 0.5 s", a.duration_s());
    let mix = LongTermSpectrum::from_stereo(&StereoSpectrum::new(a, SPECTRUM_N))?;
    let sr = a.sr as f64;
    let centres: Vec<f64> = third_octave_centres().into_iter().filter(|&fc| third_octave_edges(fc).1 <= sr / 2.0).collect();
    let level: Vec<Option<f64>> = centres
        .iter()
        .map(|&fc| {
            let (lo, hi) = third_octave_edges(fc);
            mix.band_db(lo, hi)
        })
        .collect();
    // (as `tonal` has it: bands 60 dB under the loudest are empty)
    let floor = level.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max) - 60.0;
    let level: Vec<Option<f64>> = centres
        .iter()
        .zip(level)
        .map(|(&fc, l)| {
            let (lo, hi) = third_octave_edges(fc);
            l.filter(|&l| l > floor).map(|l| l - pink_level_db(lo, hi, sr))
        })
        .collect();
    let present: Vec<f64> = level.iter().flatten().copied().collect();
    let offset = if present.is_empty() { 0.0 } else { median(&present) };
    Ok(centres.into_iter().zip(level).map(|(fc, l)| (fc, l.map(|l| l - offset))).collect())
}

fn silent_report(a: AudioRef, o: &MasterOptions, stats: &LoudnessStats) -> MasterReport {
    let mut f = Findings::default();
    f.cost(Sev::Problem, "silent", 100.0, "The master is silent (under the -70 LUFS gate): check that the tracks play, aren't muted and reach the master, then render again".into(), None);
    let issues = f.sorted();
    MasterReport {
        summary: "silent: nothing to measure; score 0/100".into(),
        score: 0,
        style: o.style,
        looped: o.looped,
        duration_s: r2(a.duration_s()),
        sample_rate: a.sr,
        loudness: loudness_part(stats, None, o),
        technical: MasterTechnical { dc_offset_dbfs: -120.0, ..MasterTechnical::default() },
        tonal: None,
        stereo: MasterStereo::default(),
        dynamics: MasterDynamics { plr_verdict: "unknown", lra_verdict: "unknown", ..MasterDynamics::default() },
        layers: None,
        seam: None,
        issues,
    }
}

fn summary(l: &MasterLoudness, issues: &[MasterIssue], score: u32) -> String {
    let opt = |v: Option<f64>, unit: &str| v.map(|v| format!("{v:.1}{unit}")).unwrap_or_else(|| "-".into());
    let n = |sev: &str| issues.iter().filter(|i| i.severity == sev).count();
    let (np, nw) = (n("problem"), n("warn"));
    let plural = |k: usize, w: &str| format!("{k} {w}{}", if k == 1 { "" } else { "s" });
    let found = if np + nw == 0 {
        "no problems".to_string()
    } else {
        let codes: Vec<&str> = issues.iter().filter(|i| i.severity != "info").take(4).map(|i| i.code.as_str()).collect();
        format!("{}, {}: {}", plural(np, "problem"), plural(nw, "warning"), codes.join(", "))
    };
    format!(
        "{} LUFS ({} LU from target), {:.1} dBTP, PLR {}, LRA {}; {found}; score {score}/100",
        opt(l.integrated_lufs, ""),
        l.from_target_lu.map(|d| format!("{d:+.1}")).unwrap_or_else(|| "-".into()),
        l.true_peak_dbtp,
        opt(l.plr_db, " dB"),
        opt(l.lra_lu, " LU"),
    )
}

/// Short-term loudness and the true peak of each 100 ms hop.
struct Series {
    hop: usize,
    short_term: Vec<f64>,
    /// Linear.
    peak: Vec<f64>,
}

fn loudness_series(a: AudioRef, looped: bool) -> Result<Series> {
    use ebur128::{EbuR128, Mode};
    let err = |e: ebur128::Error| anyhow!("loudness meter: {e:?}");
    let (n, sr) = (a.len(), a.sr as usize);
    let mut m = EbuR128::new(2, a.sr, Mode::S | Mode::TRUE_PEAK).map_err(err)?;
    if looped {
        // a loop's first windows see its end
        let k = n - n.min(3 * sr);
        m.add_frames_planar_f32(&[&a.left[k..], &a.right[k..]]).map_err(err)?;
    }
    let hop = (sr / 10).max(1);
    let (mut short_term, mut peak) = (vec![], vec![]);
    let mut i = 0;
    while i < n {
        let j = (i + hop).min(n);
        m.add_frames_planar_f32(&[&a.left[i..j], &a.right[i..j]]).map_err(err)?;
        short_term.push(m.loudness_shortterm().map_err(err)?);
        peak.push(m.prev_true_peak(0).map_err(err)?.max(m.prev_true_peak(1).map_err(err)?));
        i = j;
    }
    Ok(Series { hop, short_term, peak })
}

/// PSR over the loud passages: (smallest, where (s), median).
fn psr(s: &Series, looped: bool, sr: u32) -> (Option<f64>, Option<f64>, Option<f64>) {
    const W: usize = 30;
    let n = s.short_term.len();
    let st_max = s.short_term.iter().copied().filter(|v| v.is_finite()).fold(f64::NEG_INFINITY, f64::max);
    let mut v: Vec<(f64, usize)> = vec![];
    for i in 0..n {
        let l = s.short_term[i];
        if !l.is_finite() || l <= -70.0 || l < st_max - 20.0 || (!looped && i + 1 < W) {
            continue;
        }
        let pk = (0..W.min(n)).map(|k| s.peak[(i + n - k) % n]).fold(0.0, f64::max);
        if pk > 0.0 {
            v.push((db20(pk) - l, i));
        }
    }
    let Some(&(min, at)) = v.iter().min_by(|a, b| a.0.total_cmp(&b.0)) else {
        return (None, None, None);
    };
    let vals: Vec<f64> = v.iter().map(|x| x.0).collect();
    let at_s = (((at + 1) * s.hop) as f64 / sr as f64 - 1.5).max(0.0);
    (Some(r1(min)), Some(r1(at_s)), Some(r1(median(&vals))))
}

fn loudness_part(s: &LoudnessStats, series: Option<&Series>, o: &MasterOptions) -> MasterLoudness {
    let (psr_min_db, psr_min_at_s, psr_median_db) = series.map(|x| psr(x, o.looped, s.sample_rate)).unwrap_or((None, None, None));
    let o1 = |v: Option<f64>| v.map(r1);
    MasterLoudness {
        integrated_lufs: o1(s.integrated_lufs),
        short_term_max_lufs: o1(s.short_term_max_lufs),
        momentary_max_lufs: o1(s.momentary_max_lufs),
        lra_lu: o1(s.lra_lu),
        true_peak_dbtp: r2(s.true_peak_dbtp),
        sample_peak_dbfs: r2(s.sample_peak_dbfs),
        plr_db: o1(s.plr_db),
        psr_min_db,
        psr_min_at_s,
        psr_median_db,
        crest_db: r1(s.crest_db),
        clipped_samples: s.clipped_samples,
        target_lufs: o.target_lufs,
        ceiling_dbtp: o.ceiling_dbtp,
        from_target_lu: s.integrated_lufs.map(|l| r1(l - o.target_lufs)),
        headroom_db: r2(o.ceiling_dbtp - s.true_peak_dbtp),
    }
}

fn loudness_issues(l: &MasterLoudness, f: &mut Findings) {
    let (target, ceiling, tp) = (l.target_lufs, l.ceiling_dbtp, l.true_peak_dbtp);
    if let (Some(lufs), Some(d)) = (l.integrated_lufs, l.from_target_lu) {
        let points = (6.0 * (d.abs() - 0.5)).clamp(0.0, 25.0);
        let at_ceiling = tp >= ceiling - 0.5;
        if d.abs() > 1.0 {
            let (msg, mv) = if d < 0.0 && at_ceiling {
                (
                    format!("{lufs:.1} LUFS is {:.1} LU under the {target:.0} LUFS target with the true peak already at the ceiling ({tp:.1} dBTP): the limiter can't make it louder without crushing it", -d),
                    MasterMove::Limiter { release_ms: None, clip_db: Some(2.0) },
                )
            } else {
                let how = if d > 0.0 { "over" } else { "under" };
                (format!("{lufs:.1} LUFS is {:.1} LU {how} the {target:.0} LUFS target: it will stand out from the game's other tracks", d.abs()), MasterMove::Loudness { target_lufs: target })
            };
            f.cost(Sev::Problem, "loudness_target", points, msg, Some(mv));
        } else if d.abs() > 0.5 {
            f.add(Sev::Info, "loudness_target", points, format!("{lufs:.1} LUFS is {d:+.1} LU from the {target:.0} LUFS target"), Some(MasterMove::Loudness { target_lufs: target }));
            f.penalty += points;
        }
    }
    if tp > ceiling + 0.05 {
        let over = tp - ceiling;
        f.cost(
            Sev::Problem,
            "true_peak",
            (10.0 + 10.0 * over).min(25.0),
            format!("true peak {tp:.2} dBTP is {over:.2} dB over the {ceiling:.1} dBTP ceiling: it can clip after encoding"),
            Some(MasterMove::Limiter { release_ms: None, clip_db: Some(1.0) }),
        );
    }
    if l.clipped_samples > 0 {
        f.cost(
            Sev::Problem,
            "clipped",
            10.0,
            format!("{} samples sit at digital full scale: they are clipped; check that the master's limiter is on and its ceiling under 0 dBTP", l.clipped_samples),
            None,
        );
    }
}

/// The technical measurements (see [`MasterTechnical`]).
fn technical(a: AudioRef, spec: &StereoSpectrum) -> MasterTechnical {
    let dc = |x: &[f32]| (x.iter().map(|&v| v as f64).sum::<f64>() / x.len().max(1) as f64).abs();
    let dc = dc(a.left).max(dc(a.right));
    // from the third bin up: the first two hold the DC's own leakage
    let above = spec.band(20.0, spec.sr as f64 / 2.0).power();
    let subsonic = spec.band(spec.bin_hz(2), 20.0).power();
    MasterTechnical {
        dc_offset_dbfs: r1(db20(dc).max(-120.0)),
        subsonic_db: (above > 1e-30).then(|| r1(db10(subsonic / above).unwrap_or(-120.0).max(-120.0))),
        band_limit_hz: band_limit(spec),
    }
}

/// Where the top end is cut off (see [`MasterTechnical::band_limit_hz`]): the first 1/6-octave step from 4 kHz up
/// past which every step stays 30 dB under the loudest step of the third of an octave below it, refined to the
/// highest bin around it that still reaches that level. A low-pass in a resampler or an encoder falls that far that
/// fast; a dark mix rolls off over octaves (the corpus' darkest fall about 30 dB per octave) and isn't cut off.
fn band_limit(spec: &StereoSpectrum) -> Option<f64> {
    let top = (0.45 * spec.sr as f64).min(20000.0);
    let step = 2f64.powf(1.0 / 6.0);
    let mut steps: Vec<(f64, f64)> = vec![];
    let mut lo = 2000.0;
    while lo < top {
        let b = spec.band(lo, (lo * step).min(top));
        if b.bins > 0 {
            steps.push((lo, b.power() / b.bins as f64));
        }
        lo *= step;
    }
    let below = |f: f64| steps.iter().filter(|s| s.0 >= f / CLIFF_OCTAVES.exp2() - 1.0 && s.0 < f).map(|s| s.1).fold(0.0, f64::max);
    let i = (0..steps.len()).find(|&i| {
        let f = steps[i].0;
        let above = steps[i..].iter().map(|s| s.1).fold(0.0, f64::max);
        f >= 3999.0 && below(f) > 1e-30 && above < below(f) * 1e-3
    })?;
    let f = steps[i].0;
    let floor = below(f) * 1e-3;
    let k0 = (f / step / spec.bin_hz(1)).floor() as usize;
    let k1 = ((f * step / spec.bin_hz(1)).ceil() as usize).min(spec.ll.len() - 1);
    let k = (k0..=k1).rev().find(|&k| spec.power(k) >= floor).unwrap_or(k0);
    Some((spec.bin_hz(k + 1) / 10.0).round() * 10.0)
}

/// How quickly the top end has to fall 30 dB to be cut off (octaves).
const CLIFF_OCTAVES: f64 = 1.0 / 3.0;
/// A DC offset over this is reported (dBFS): the corpus' largest is -44.
const DC_OFFSET_DBFS: f64 = -40.0;
/// Power under 20 Hz over this, against the power above, is reported (dB): the corpus' masters stay under -16, most
/// under -40.
const SUBSONIC_DB: f64 = -12.0;
/// A top end cut off under this is a warning, with the exciter as its move (Hz).
const BAND_LIMIT_WARN_HZ: f64 = 15000.0;
/// One cut off under this is noted (Hz): a full-range master reaches 18 to 20 kHz.
const BAND_LIMIT_NOTE_HZ: f64 = 17000.0;

fn technical_issues(t: &MasterTechnical, f: &mut Findings) {
    if t.dc_offset_dbfs > DC_OFFSET_DBFS {
        f.cost(
            Sev::Warn,
            "dc_offset",
            3.0,
            format!(
                "The master carries a DC offset ({:.0} dBFS): the waveform sits off centre, which costs headroom and clicks where playback starts and stops",
                t.dc_offset_dbfs
            ),
            Some(MasterMove::HighPass { hz: 10.0, slope: 12 }),
        );
    }
    if let Some(s) = t.subsonic_db
        && s > SUBSONIC_DB
    {
        f.cost(
            Sev::Warn,
            "subsonic",
            3.0,
            format!("Rumble under 20 Hz sits only {:.0} dB under the rest: no speaker plays it, but it costs headroom and makes the limiter work", -s),
            Some(MasterMove::HighPass { hz: 20.0, slope: 24 }),
        );
    }
    if let Some(hz) = t.band_limit_hz {
        let found = format!(
            "The top end stops at {} Hz: above it the spectrum drops 30 dB within a third of an octave, as after a low-bitrate file or a \
resampled one (a sample set recorded at a low rate, an MP3 among the clips; spectrogram shows where it starts)",
            hz_text(hz)
        );
        if hz < BAND_LIMIT_WARN_HZ {
            let mv = MasterMove::Exciter { hz_lo: EXCITER_LO_HZ, hz_hi: EXCITER_HI_HZ, mix: EXCITER_MIX, mode: Saturation::Tube };
            f.cost(Sev::Warn, "band_limited", 3.0, format!("{found}. Next to full-range songs it sounds muffled; replacing the source is best, the exciter's harmonics bring back some air"), Some(mv));
        } else if hz < BAND_LIMIT_NOTE_HZ {
            f.add(Sev::Info, "band_limited", 0.0, format!("{found}. Few listeners hear above it, but a full-range master reaches 18 to 20 kHz"), None);
        }
    }
}

/// The exciter band a cut-off top end gets: its harmonics land above the cut (Hz). On 99 corpus excerpts low-passed
/// at 11 kHz, half of 2.5-8 kHz through the tube curve brought back about half of what the cut took from 14 to 18 kHz
/// (20 of 38 dB) for 1 dB off 4-8 kHz, and lifted the cut-off in nine excerpts of ten; the tape curve did a little
/// less, and the band above 8 kHz little at all (its harmonics fall past the Nyquist frequency).
const EXCITER_LO_HZ: f64 = 2500.0;
const EXCITER_HI_HZ: f64 = 8000.0;
/// How much of that band the exciter saturates.
const EXCITER_MIX: f64 = 0.5;

/// Tonal regions: (name, low edge, high edge, code when too much, code when too little).
pub(crate) const REGIONS: [(&str, f64, f64, &str, &str); 9] = [
    ("sub", 20.0, 40.0, "sub_heavy", "sub_light"),
    ("boom", 40.0, 100.0, "boomy", "lacks_weight"),
    ("warmth", 100.0, 250.0, "heavy", "thin"),
    ("mud", 200.0, 500.0, "muddy", "hollow"),
    ("boxy", 500.0, 1000.0, "boxy", "scooped"),
    ("honk", 1000.0, 2000.0, "honky", "recessed"),
    ("harsh", 2000.0, 5000.0, "harsh", "lacks_presence"),
    ("sibilance", 5000.0, 10000.0, "sibilant", "dull"),
    ("air", 10000.0, 20000.0, "fizzy", "lacks_air"),
];

/// What a region's issue code means: (what the region is, what the listener hears).
fn region_words(code: &str) -> (&'static str, &'static str) {
    match code {
        "sub_heavy" => ("deep sub", "it eats headroom, and small speakers can't play it"),
        "sub_light" => ("deep sub", "fine on phones and laptops, thin on big speakers"),
        "boomy" => ("bass", "boomy, and it eats headroom"),
        "lacks_weight" => ("bass", "the mix lacks weight"),
        "heavy" => ("upper bass", "heavy and thick"),
        "thin" => ("upper bass", "thin, without warmth"),
        "muddy" => ("low mids", "muddy: parts blur together"),
        "hollow" => ("low mids", "hollow"),
        "boxy" => ("lower mids", "boxy"),
        "scooped" => ("lower mids", "scooped: instruments lose body"),
        "honky" => ("mids", "honky and nasal"),
        "recessed" => ("mids", "melodies and leads sit back"),
        "harsh" => ("upper mids", "harsh and tiring, where the ear is most sensitive"),
        "lacks_presence" => ("upper mids", "it lacks presence and definition"),
        "sibilant" => ("presence highs", "sibilant, with splashy cymbals"),
        "dull" => ("presence highs", "dull"),
        "fizzy" => ("air", "fizzy and brittle"),
        _ => ("air", "it lacks air"),
    }
}

/// How far past its tolerance a region has to be before the report sends the agent to the mix (dB): two of the
/// master's EQ moves.
const FIX_IN_MIX_DB: f64 = 6.0;

/// One EQ band for a region the fitted correction left without one: across the region (a shelf at either end of the
/// spectrum), bringing it back to within half the tolerance, at most 3 dB. None for a missing sub: EQ can't create
/// it, and the boost would cost headroom.
fn region_eq(lo: f64, hi: f64, d: f64, t: f64) -> Option<MasterMove> {
    if d < 0.0 && hi <= 40.0 {
        return None;
    }
    let db = r1(-d.signum() * (d.abs() - t / 2.0).clamp(0.5, 3.0));
    let (shape, hz, q) = region_band(lo, hi);
    Some(MasterMove::Eq { shape, hz, db, q, part: Part::Stereo })
}

/// A band across the region `lo..hi`: a shelf at either end of the spectrum, else a peak at its centre as wide as
/// the region (Q = centre / bandwidth, within the moves' 0.7 to 1.4).
fn region_band(lo: f64, hi: f64) -> (EqShape, f64, f64) {
    if hi <= 40.0 {
        (EqShape::LowShelf, hi, 0.7)
    } else if lo >= 10000.0 {
        (EqShape::HighShelf, lo, 0.7)
    } else {
        let c = (lo * hi).sqrt();
        (EqShape::Peak, if c >= 1000.0 { (c / 10.0).round() * 10.0 } else { c.round() }, r2((c / (hi - lo)).clamp(0.7, 1.4)))
    }
}

/// A region whose share of the frames' loudest moments stands this far over its usual (its `variability_db`) is
/// too loud only some of the time: its move is a dynamic EQ band, not a cut. Per region in [`REGIONS`]' order: what
/// three in four of the corpus' masters stay under (the bass and the highs come and go far more than the mids).
const VARIABLE_DB: [f64; 9] = [14.5, 8.5, 4.7, 4.7, 6.2, 7.3, 7.5, 10.3, 12.2];
/// How far a dynamic EQ band's range has to reach for each dB the region's average should come down: on 99 corpus
/// excerpts a band from 4 dB under the region's loud level, ratio 3, brought the region's average down by a third of
/// its range (1.3 dB for 4).
const DYN_RANGE_PER_DB: f64 = 3.0;

/// The dynamic EQ band for a region `d` dB over the target that comes and goes: across the region, reaching far
/// enough to bring it back to within half the tolerance `t` (2 to 6 dB).
fn region_dyn_eq(lo: f64, hi: f64, d: f64, t: f64) -> MasterMove {
    let (shape, hz, q) = region_band(lo, hi);
    MasterMove::DynEq { shape, hz, q, range_db: -r1((DYN_RANGE_PER_DB * (d - t / 2.0)).clamp(2.0, 6.0)) }
}

/// The most a move turns the side signal up or down (dB): it's a part of the region's power, so it takes more.
const SIDE_MAX_DB: f64 = 4.5;

/// The side-only gain that changes a region's power by `db`, its side `side_mid_db` under its mid; None when even
/// [`SIDE_MAX_DB`] on the side gets less than half of `db`.
fn side_gain(db: f64, side_mid_db: f64) -> Option<f64> {
    let s = 1.0 / (1.0 + 10f64.powf(-side_mid_db / 10.0));
    let total = |g: f64| 10.0 * (1.0 - s + s * 10f64.powf(g / 10.0)).log10();
    let want = 10f64.powf(db / 10.0) - 1.0 + s;
    let g = if want > 0.0 { 10.0 * (want / s).log10() } else { -SIDE_MAX_DB };
    let g = r1(g.clamp(-SIDE_MAX_DB, SIDE_MAX_DB));
    (g.abs() >= 0.5 && total(g).abs() >= db.abs() / 2.0).then_some(g)
}

/// A 1/3-octave band centred on `fc` belongs to the region `lo..hi`.
fn in_region(fc: f64, lo: f64, hi: f64) -> bool {
    let f = fc * 1.01;
    f >= lo && f < hi
}

/// Whether an EQ band acts on the region `lo..hi`.
fn covers(b: &EqBand, lo: f64, hi: f64) -> bool {
    match b.shape {
        EqShape::Peak => b.hz >= lo / 1.26 && b.hz <= hi * 1.26,
        EqShape::LowShelf => lo < b.hz * 1.5,
        EqShape::HighShelf => hi > b.hz / 1.5,
    }
}

/// Linear interpolation in `pts` (x ascending), held at the ends.
fn interp(pts: &[(f64, f64)], x: f64) -> f64 {
    match pts {
        [] => 0.0,
        [p, ..] if x <= p.0 => p.1,
        [.., p] if x >= p.0 => p.1,
        _ => {
            let j = pts.partition_point(|p| p.0 <= x).clamp(1, pts.len() - 1);
            let (a, b) = (pts[j - 1], pts[j]);
            a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn tonal(
    mix: &LongTermSpectrum,
    spec: &StereoSpectrum,
    reference: Option<(&LongTermSpectrum, &StereoSpectrum)>,
    style: TonalStyle,
    variability: &[Option<f64>],
    bursts: Option<&Bursts>,
    band_limit: Option<f64>,
    f: &mut Findings,
) -> TonalBalance {
    let (reference, ref_spec) = (reference.map(|r| r.0), reference.map(|r| r.1));
    let sr = mix.sr as f64;
    let centres: Vec<f64> = third_octave_centres().into_iter().filter(|&fc| third_octave_edges(fc).1 <= sr / 2.0).collect();
    let level: Vec<Option<f64>> = centres
        .iter()
        .map(|&fc| {
            let (lo, hi) = third_octave_edges(fc);
            mix.band_db(lo, hi)
        })
        .collect();
    let target: Vec<Option<f64>> = centres
        .iter()
        .map(|&fc| {
            let (lo, hi) = third_octave_edges(fc);
            match reference {
                // a reference at a lower sample rate has nothing to say above its Nyquist
                Some(r) => (hi <= r.sr as f64 / 2.0).then(|| r.band_db(lo, hi)).flatten(),
                None => Some(pink_level_db(lo, hi, sr) + target_db(style, fc)),
            }
        })
        .collect();
    // bands 60 dB under the loudest are empty, not out of balance
    let floor = |v: &[Option<f64>]| v.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max) - 60.0;
    let (fl, ft) = (floor(&level), floor(&target));
    let raw: Vec<Option<f64>> = level
        .iter()
        .zip(&target)
        .map(|(l, t)| match (l, t) {
            (Some(l), Some(t)) if *l > fl && *t > ft => Some(l - t),
            _ => None,
        })
        .collect();
    let (offset, dev) = deviations(&raw);
    // how close each style's target comes from 100 Hz up: the agent may have asked for the wrong one
    let rms = |dev: &[Option<f64>]| r2(mean(&dev.iter().flatten().map(|d| d * d).collect::<Vec<_>>()).unwrap_or(0.0).sqrt());
    let styles: Vec<StyleFit> = if reference.is_some() {
        vec![]
    } else {
        TonalStyle::ALL
            .iter()
            .map(|&s| {
                let raw: Vec<Option<f64>> = centres
                    .iter()
                    .zip(&level)
                    .map(|(&fc, l)| {
                        let (lo, hi) = third_octave_edges(fc);
                        l.filter(|&l| l > fl && fc >= STYLE_MATCH_FROM_HZ).map(|l| l - pink_level_db(lo, hi, sr) - target_db(s, fc))
                    })
                    .collect();
                StyleFit { style: s, deviation_rms_db: rms(&deviations(&raw).1) }
            })
            .collect()
    };
    let closest = styles.iter().min_by(|a, b| a.deviation_rms_db.total_cmp(&b.deviation_rms_db));
    if let (Some(best), Some(asked)) = (closest, styles.iter().find(|x| x.style == style))
        && best.style != style
        && asked.deviation_rms_db - best.deviation_rms_db >= STYLE_HINT_DB
    {
        f.add(
            Sev::Info,
            "closer_style",
            0.0,
            format!(
                "From 100 Hz up, the balance is closer to the {} target ({:.1} dB RMS off it) than the {} one ({:.1} dB): if the song is {}, check it with style {}",
                best.style.name(),
                best.deviation_rms_db,
                style.name(),
                asked.deviation_rms_db,
                best.style.music(),
                best.style.name()
            ),
            None,
        );
    }
    let closest_style = closest.map(|x| x.style);
    let band_tol = |fc: f64| if reference.is_some() { reference_tolerance_db(fc) } else { tolerance_db(style, fc) };
    let tol: Vec<f64> = centres.iter().map(|&fc| band_tol(fc)).collect();
    let excess = |i: usize| dev[i].map_or(0.0, |d| (d.abs() - tol[i]).clamp(0.0, 6.0));

    // regions
    let mut regions = vec![];
    let mut region_dev: Vec<Option<(f64, f64)>> = vec![];
    for (ri, &(name, lo, hi, high, low)) in REGIONS.iter().enumerate() {
        let idx: Vec<usize> = (0..centres.len()).filter(|&i| in_region(centres[i], lo, hi) && dev[i].is_some()).collect();
        let d = mean(&idx.iter().filter_map(|&i| dev[i]).collect::<Vec<_>>());
        let t = match reference {
            Some(_) => mean(&idx.iter().map(|&i| tol[i]).collect::<Vec<_>>()).unwrap_or_else(|| band_tol((lo * hi).sqrt())),
            None => region_tolerance_db(style, ri),
        };
        let (verdict, code) = match d {
            Some(x) if x > t => ("high", Some(high)),
            Some(x) if x < -t => ("low", Some(low)),
            _ => ("ok", None),
        };
        region_dev.push(d.map(|d| (d, t)));
        regions.push(TonalRegion {
            region: name,
            lo_hz: lo,
            hi_hz: hi,
            deviation_db: d.map(r1),
            tolerance_db: r1(t),
            verdict,
            code,
            variability_db: variability.get(ri).copied().flatten().map(r1),
            side_mid_db: side_mid(spec, lo, hi),
        });
    }
    let flagged: Vec<usize> = (0..REGIONS.len()).filter(|&ri| regions[ri].code.is_some()).collect();

    // the correction: flagged regions' bands back to within half the tolerance (a sub that isn't there is not
    // boosted: EQ can't create it, and it would cost headroom)
    let corr: Vec<Option<f64>> = (0..centres.len())
        .map(|i| {
            let d = dev[i]?;
            let wanted = flagged.iter().any(|&ri| {
                let (_, lo, hi, ..) = REGIONS[ri];
                in_region(centres[i], lo, hi) && (regions[ri].verdict == "high") == (d > 0.0)
            });
            if !wanted || d.abs() <= tol[i] / 2.0 || (d < 0.0 && centres[i] < 40.0) {
                return Some(0.0);
            }
            Some(-d.signum() * (d.abs() - tol[i] / 2.0))
        })
        .collect();
    let pts: Vec<(f64, f64)> = centres.iter().zip(&corr).filter_map(|(&fc, c)| c.map(|c| (fc.ln(), c))).collect();
    let grid = log_grid(25.0, 16000f64.min(0.45 * sr), 12);
    let curve: Vec<f64> = grid.iter().map(|&g| interp(&pts, g.ln())).collect();
    let fo = FitOptions { max_bands: 4, max_db: 3.0, q_min: 0.7, q_max: 1.4, min_db: 0.5, ..FitOptions::default() };
    let correction = if curve.iter().any(|v| v.abs() > 0.0) { fit_eq(&grid, &curve, sr, &fo) } else { vec![] };

    // sibilance bursts against what the target's balance would give
    let power = |i: usize| target[i].map_or(0.0, |t| 10f64.powf(t / 10.0));
    let total: f64 = (0..centres.len()).map(power).sum();
    let sib_share: f64 = (0..centres.len()).filter(|&i| in_region(centres[i], 5000.0, 10000.0)).map(power).sum();
    let sibilance = bursts.zip(db10(sib_share / total)).map(|(b, expected)| SibilanceBursts {
        spike_db: r1(b.spike_db),
        burst_db: r1(b.burst_db),
        expected_db: r1(expected),
        bursty: b.burst_db >= BURST_DB && b.spike_db >= expected + SPIKE_DB,
        centre_hz: b.centre_hz.map(|c| c.round()),
    });
    let de_ess = sibilance.as_ref().filter(|s| s.bursty).map(|s| MasterMove::DeEss {
        hz: ((s.centre_hz.unwrap_or(7000.0) * 0.8).clamp(4000.0, 10000.0) / 100.0).round() * 100.0,
        range_db: -r1((s.spike_db - (s.expected_db + SPIKE_DB - 3.0)).clamp(3.0, 6.0)),
    });

    // the issues, the furthest out first, each taking the correction band that acts on it
    let target_name = if reference.is_some() { "reference" } else { style.name() };
    let mut order = flagged;
    let out = |ri: usize| region_dev[ri].map_or(0.0, |(d, t)| d.abs() - t);
    order.sort_by(|&a, &b| out(b).total_cmp(&out(a)));
    let mut used = vec![false; correction.len()];
    let mut de_ess_used = false;
    let has = |code: &str| regions.iter().any(|r| r.code == Some(code));
    let mut points = 0.0;
    let mut counted = vec![false; centres.len()];
    for ri in order {
        let (_, lo, hi, ..) = REGIONS[ri];
        let Some((d, t)) = region_dev[ri] else { continue };
        let Some(code) = regions[ri].code else { continue };
        let members: Vec<usize> = (0..centres.len()).filter(|&i| in_region(centres[i], lo, hi)).collect();
        for &i in &members {
            if !counted[i] {
                counted[i] = true;
                points += 0.6 * excess(i);
            }
        }
        if code == "lacks_air" && has("dull") {
            continue;
        }
        let high = d > 0.0;
        let over = d.abs() - t;
        // a problem one of the corpus' spreads past the tolerance (4 dB past a reference's)
        let problem = if reference.is_some() { 4.0 } else { (t / TOLERANCE_SPREADS).max(2.0) };
        let mut sev = if over > problem { Sev::Problem } else { Sev::Warn };
        if code == "sub_light" || code == "lacks_air" {
            sev = sev.milder();
        }
        let weight = 0.6 * members.iter().map(|&i| excess(i)).sum::<f64>();
        let var = regions[ri].variability_db;
        // a top end that is cut off can't be brought back with EQ: the band_limited issue has the exciter
        let cut_off = !high && band_limit.is_some_and(|b| b < hi * 0.8);
        let mut side_note = None;
        let suggestion = if code == "sibilant" && de_ess.is_some() {
            de_ess_used = true;
            de_ess.clone()
        } else if cut_off {
            None
        } else if high && var.is_some_and(|v| v >= VARIABLE_DB[ri]) {
            Some(region_dyn_eq(lo, hi, d, t))
        } else {
            let pick = (0..correction.len())
                .filter(|&j| !used[j] && (correction[j].db < 0.0) == high && covers(&correction[j], lo, hi))
                .max_by(|&x, &y| correction[x].db.abs().total_cmp(&correction[y].db.abs()));
            let mv = pick
                .map(|j| {
                    used[j] = true;
                    let b = correction[j];
                    MasterMove::Eq { shape: b.shape, hz: b.hz, db: b.db, q: b.q, part: Part::Stereo }
                })
                .or_else(|| region_eq(lo, hi, d, t));
            // when the excess or the lack is in the sides, the move works on the side signal and leaves the centre
            match (mv, regions[ri].side_mid_db, side_target(style, ref_spec, ri)) {
                (Some(MasterMove::Eq { shape, hz, db, q, .. }), Some(sm), Some((med, tol)))
                    if (high && sm > med + tol) || (!high && sm < med - tol) =>
                {
                    match side_gain(db, sm) {
                        Some(g) => {
                            side_note = Some((sm, med));
                            Some(MasterMove::Eq { shape, hz, db: g, q, part: Part::Side })
                        }
                        None => Some(MasterMove::Eq { shape, hz, db, q, part: Part::Stereo }),
                    }
                }
                (mv, ..) => mv,
            }
        };
        let (what, hears) = region_words(code);
        let hears = if code == "dull" && has("lacks_air") { "dull, and the air above 10 kHz is missing too" } else { hears };
        let mut msg = format!(
            "The {}-{} Hz range ({what}) sits {d:+.1} dB from the {target_name} target (tolerance +/-{t:.1} dB): {hears}",
            hz_text(lo),
            hz_text(hi)
        );
        if let Some(MasterMove::DynEq { .. }) = &suggestion
            && let Some(v) = var
        {
            msg.push_str(&format!("; it comes and goes ({v:.0} dB between its usual and its loud moments), so tame it only when it comes up"));
        }
        if let Some((sm, med)) = side_note {
            let more = if high { "more" } else { "less" };
            msg.push_str(&format!(
                "; the sides carry {more} of it than the {target_name} masters do (side {:.1} dB under mid there, against {:.1}), so the move works on the side signal and leaves the centre alone",
                -sm, -med
            ));
        }
        if cut_off && let Some(b) = band_limit {
            msg.push_str(&format!("; the top end is cut off at {} Hz (see band_limited), and EQ can't bring back what isn't there", hz_text(b)));
        }
        // past what a few small master moves can bring back, the balance is the arrangement's or the mix's
        if over > FIX_IN_MIX_DB && code != "sub_light" && code != "lacks_air" {
            let fix = if high {
                "turn down the tracks playing there (mix_check's regions name them)"
            } else {
                "bring up the tracks playing there (mix_check's regions name them) or give the range a part of its own"
            };
            msg.push_str(&format!("; that is {over:.0} dB past the tolerance, more than the master should fix (its EQ moves stop at 3 dB): {fix}"));
        }
        f.add(sev, code, weight, msg, suggestion);
    }
    f.penalty += points.min(30.0);
    if let Some(s) = sibilance.as_ref().filter(|s| s.bursty && !de_ess_used) {
        let sev = if s.spike_db >= s.expected_db + SPIKE_WARN_DB { Sev::Warn } else { Sev::Info };
        let points = if sev == Sev::Warn { 5.0 } else { 0.0 };
        f.cost(
            sev,
            "sibilance_bursts",
            points,
            format!(
                "Bursts of 5-10 kHz stand {:.0} dB out of the rest (their share reaches {:.0} dB, the target's is {:.0} dB): sibilants or splashes that a static EQ would only dull",
                s.burst_db, s.spike_db, s.expected_db
            ),
            de_ess,
        );
    }
    let bands = (0..centres.len())
        .map(|i| TonalBand {
            band: third_octave_label(centres[i]),
            centre_hz: r1(centres[i]),
            level_db: level[i].map(r1),
            target_db: target[i].map(|t| r1(t + offset)),
            deviation_db: dev[i].map(r1),
            tolerance_db: r1(tol[i]),
            outside: dev[i].is_some_and(|d| d.abs() > tol[i]),
        })
        .collect();
    TonalBalance {
        target: target_name,
        offset_db: r1(offset),
        deviation_rms_db: rms(&dev),
        bands,
        regions,
        sibilance,
        correction,
        styles,
        closest_style,
    }
}

/// How much closer another style's target has to be before the report mentions it (dB RMS).
const STYLE_HINT_DB: f64 = 1.0;

/// The style match leaves out the bands under this (Hz): masters of one style differ most there, and a missing low
/// end is a fault to fix, not a style (matched on it, every song without bass looks acoustic).
const STYLE_MATCH_FROM_HZ: f64 = 100.0;

/// Deviations from a target band by band (`None` where a band is empty): the median over the bands taken off
/// (returned as the offset; the overall level is the loudness stage's job), each smoothed with its neighbours.
fn deviations(raw: &[Option<f64>]) -> (f64, Vec<Option<f64>>) {
    let present: Vec<f64> = raw.iter().flatten().copied().collect();
    let offset = if present.is_empty() { 0.0 } else { median(&present) };
    let dev = (0..raw.len())
        .map(|i| {
            raw[i]?;
            let (mut s, mut w) = (0.0, 0.0);
            for (j, k) in [(i.wrapping_sub(1), 0.25), (i, 0.5), (i + 1, 0.25)] {
                if let Some(Some(v)) = raw.get(j) {
                    s += k * (v - offset);
                    w += k;
                }
            }
            Some(s / w)
        })
        .collect();
    (offset, dev)
}

/// Sibilance is bursty when its 99th percentile stands this far over its median (dB)...
const BURST_DB: f64 = 12.0;
/// ...and this far over the target's share of 5-10 kHz (dB): about one master in fourteen of the corpus.
const SPIKE_DB: f64 = 15.0;
/// Bursts this far over the target's share are a warning (dB): about one master in thirty.
const SPIKE_WARN_DB: f64 = 17.0;
/// The threshold of a de-esser a [`MasterMove::DeEss`] adds to the master, over its band's loud level (dB): it
/// catches the loudest moments only. On 99 masters of the corpus a de-esser above 6 kHz with a range of 6 dB took
/// the bursts down 1.2 dB at this threshold and 2.1 dB at -6 dB, and the air with them by 2.3 and 3.8 dB: a deeper
/// master de-esser dulls the whole top, so the rest belongs on the tracks.
pub const DEESS_THRESHOLD_DB: f64 = 0.0;

/// Bursts of 5-10 kHz in 21 ms frames of the mid signal.
struct Bursts {
    /// 99th percentile of the band's share of each frame, dB.
    spike_db: f64,
    /// `spike_db` minus the median share, dB.
    burst_db: f64,
    /// Centre of the 4-12 kHz content in the frames at or above the 97th percentile share, Hz.
    centre_hz: Option<f64>,
}

/// Power of a Hann-windowed frame of the mid signal per bin.
fn frame_power(a: AudioRef, start: usize, win: &[f64]) -> Vec<f64> {
    let x: Vec<f64> = (0..win.len()).map(|j| a.mid(start + j) * win[j]).collect();
    rfft(&x, win.len()).iter().map(|c| c.norm_sqr()).collect()
}

/// Frames within 30 dB of the loud ones (the 95th percentile of frame power).
fn active(totals: &[f64]) -> Option<f64> {
    let loud = percentile(totals, 95.0);
    (loud.is_finite() && loud > 1e-20).then_some(loud * 1e-3)
}

fn sibilance_bursts(a: AudioRef) -> Option<Bursts> {
    const N: usize = 1024;
    let sr = a.sr as f64;
    if sr / 2.0 <= 10000.0 || a.len() < N {
        return None;
    }
    let win = hann_periodic(N);
    let bin = sr / N as f64;
    let k = |f: f64| ((f / bin).ceil() as usize).min(N / 2 + 1);
    let (s0, s1, c0, c1) = (k(5000.0), k(10000.0), k(4000.0), k(12000.0));
    let frames = (a.len() - N) / (N / 2) + 1;
    let per: Vec<(f64, f64, f64, f64)> = (0..frames)
        .into_par_iter()
        .map(|t| {
            let p = frame_power(a, t * N / 2, &win);
            let (mut num, mut den) = (0.0, 0.0);
            for (j, v) in p.iter().enumerate().take(c1).skip(c0) {
                num += j as f64 * bin * v;
                den += v;
            }
            (p[1..].iter().sum(), p[s0..s1].iter().sum(), num, den)
        })
        .collect();
    let gate = active(&per.iter().map(|v| v.0).collect::<Vec<_>>())?;
    let on: Vec<&(f64, f64, f64, f64)> = per.iter().filter(|v| v.0 >= gate && v.1 > 0.0).collect();
    if on.len() < 20 {
        return None;
    }
    let r: Vec<f64> = on.iter().map(|v| 10.0 * (v.1 / v.0).log10()).collect();
    let spike = percentile(&r, 99.0);
    let top = percentile(&r, 97.0);
    let (num, den) = on.iter().zip(&r).filter(|(_, x)| **x >= top).fold((0.0, 0.0), |s, (v, _)| (s.0 + v.2, s.1 + v.3));
    Some(Bursts { spike_db: spike, burst_db: spike - median(&r), centre_hz: (den > 0.0).then(|| num / den) })
}

/// Per tonal region, how much its share of the signal comes and goes: the 90th minus the 50th percentile of its
/// share of each 170 ms frame of the mid signal, dB.
fn region_variability(a: AudioRef) -> Vec<Option<f64>> {
    const N: usize = 8192;
    if a.len() < N {
        return vec![None; REGIONS.len()];
    }
    let win = hann_periodic(N);
    let bin = a.sr as f64 / N as f64;
    let k = |f: f64| ((f / bin).ceil() as usize).min(N / 2 + 1);
    let ranges: Vec<(usize, usize)> = REGIONS.iter().map(|r| (k(r.1), k(r.2))).collect();
    let frames = (a.len() - N) / (N / 2) + 1;
    let per: Vec<(f64, Vec<f64>)> = (0..frames)
        .into_par_iter()
        .map(|t| {
            let p = frame_power(a, t * N / 2, &win);
            (p[1..].iter().sum(), ranges.iter().map(|&(x, y)| p[x..y].iter().sum()).collect())
        })
        .collect();
    let Some(gate) = active(&per.iter().map(|v| v.0).collect::<Vec<_>>()) else {
        return vec![None; REGIONS.len()];
    };
    (0..REGIONS.len())
        .map(|ri| {
            let v: Vec<f64> = per.iter().filter(|(t, p)| *t >= gate && p[ri] > 0.0).map(|(t, p)| 10.0 * (p[ri] / t).log10()).collect();
            (v.len() >= 4).then(|| percentile(&v, 90.0) - percentile(&v, 50.0))
        })
        .collect()
}

/// The four stereo bands.
const STEREO_BANDS: [(&str, f64, f64); 4] = [("low", 20.0, 150.0), ("low_mid", 150.0, 800.0), ("mid", 800.0, 5000.0), ("high", 5000.0, 20000.0)];

/// The edges between the stereo bands (Hz): an imager split here works on the bands the report measures.
pub const STEREO_CROSSOVERS_HZ: [f64; 3] = [150.0, 800.0, 5000.0];

/// Side over mid power in `lo..hi` Hz (up to the Nyquist frequency), dB: -120 for a mono band, `None` for silence.
fn side_mid(spec: &StereoSpectrum, lo: f64, hi: f64) -> Option<f64> {
    let s = spec.band(lo, hi.min(spec.sr as f64 / 2.0));
    if s.mid() > 1e-30 { db10(s.side() / s.mid()).map(|x| r1(x.max(-120.0))).or(Some(-120.0)) } else { None }
}

/// The side over mid a tonal region's side should have, with how far it may stray before a move on the region goes
/// to the side signal alone (dB): the reference's own and 3 dB, else the style's corpus median and one spread (at
/// least 2 dB). Not an issue of its own, only where the move goes, so it doesn't wait for 2.5 spreads.
fn side_target(style: TonalStyle, reference: Option<&StereoSpectrum>, ri: usize) -> Option<(f64, f64)> {
    let (_, lo, hi, ..) = REGIONS[ri];
    match reference {
        Some(r) => side_mid(r, lo, hi).map(|x| (x, REFERENCE_WIDTH_TOLERANCE_DB)),
        None => {
            let m = style.measured();
            Some((m.side_median[ri], m.side_spread[ri].max(2.0)))
        }
    }
}

/// How far a band's side over mid may sit from a reference's (dB).
const REFERENCE_WIDTH_TOLERANCE_DB: f64 = 3.0;
/// A band whose side sits further under its mid than this has next to nothing for an imager to widen (dB).
const WIDEN_FROM_DB: f64 = -24.0;

/// The four stereo bands of a spectrum (those under the Nyquist frequency).
fn stereo_bands(spec: &StereoSpectrum) -> Vec<StereoRegion> {
    let k_total = spec.k_weighted_power();
    STEREO_BANDS
        .iter()
        .filter(|b| b.1 < spec.sr as f64 / 2.0)
        .map(|&(band, lo, hi)| {
            let s = spec.band(lo, hi);
            StereoRegion {
                band,
                lo_hz: lo,
                hi_hz: hi,
                level_db: db10(s.power() / k_total).map(r1),
                correlation: s.correlation().map(r2),
                side_mid_db: side_mid(spec, lo, hi),
                width: s.width().map(r2),
            }
        })
        .collect()
}

/// The low-mid, mid and high bands' width against the style's corpus or the reference: an imager move for each band
/// outside it. Returns whether a band is too wide.
fn band_widths(bands: &[StereoRegion], reference: Option<&[StereoRegion]>, style: TonalStyle, skip: Option<&str>, f: &mut Findings) -> bool {
    let mut wide = false;
    let target_name = if reference.is_some() { "reference" } else { style.name() };
    for (i, b) in bands.iter().enumerate().skip(1) {
        let (Some(sm), true) = (b.side_mid_db, b.level_db.is_some_and(|l| l >= -30.0)) else { continue };
        if skip == Some(b.band) {
            continue;
        }
        let (med, tol) = match reference {
            // a reference nearly mono in a band says nothing about the width there
            Some(r) => match r.get(i).and_then(|x| x.side_mid_db) {
                Some(x) if x > WIDEN_FROM_DB => (x, REFERENCE_WIDTH_TOLERANCE_DB),
                _ => continue,
            },
            None => {
                let m = style.measured();
                (m.width_median[i - 1], (TOLERANCE_SPREADS * m.width_spread[i - 1]).max(2.0))
            }
        };
        let name = b.band.replace('_', "-");
        let range = format!("{}-{} Hz", hz_text(b.lo_hz), hz_text(b.hi_hz));
        if sm > med + tol {
            wide = true;
            // the side scaled by w moves side over mid by 20 log10(w): back to within half the tolerance
            let w = r2(10f64.powf(-(sm - (med + tol / 2.0)) / 20.0).clamp(0.5, 0.95));
            f.cost(
                Sev::Warn,
                "band_wide",
                2.0,
                format!(
                    "The {name} band ({range}) is wider than the {target_name} masters: its side sits {:.1} dB under its mid, against {:.1} (tolerance +/-{tol:.1} dB). Parts there turn thin and phasey on phones and in mono",
                    -sm, -med
                ),
                Some(MasterMove::Imager { hz_lo: b.lo_hz, hz_hi: b.hi_hz, width: w }),
            );
        } else if sm < med - tol && sm > WIDEN_FROM_DB {
            let w = r2(10f64.powf((med - tol / 2.0 - sm) / 20.0).clamp(1.05, 1.4));
            f.add(
                Sev::Info,
                "band_narrow",
                0.0,
                format!(
                    "The {name} band ({range}) is narrower than the {target_name} masters: its side sits {:.1} dB under its mid, against {:.1} (tolerance +/-{tol:.1} dB), so the song may feel small next to them",
                    -sm, -med
                ),
                Some(MasterMove::Imager { hz_lo: b.lo_hz, hz_hi: b.hi_hz, width: w }),
            );
        }
    }
    wide
}

/// Per stereo band, the left/right correlation under which it is a phase warning and a phase problem. Reverb and
/// wide synths decorrelate the highs, so a band's limits fall the higher it is: about one master in twelve of the
/// corpus goes under a warning limit somewhere, and one in thirty under a problem's, most of those with a channel's
/// polarity flipped.
const PHASE_LIMITS: [(f64, f64); 4] = [(0.1, -0.1), (-0.1, -0.3), (-0.25, -0.5), (-0.35, -0.6)];

/// The phase limits (warning, problem) of the stereo band named `band` (as in [`StereoRegion::band`]): under them its
/// correlation is a phase warning or problem.
pub fn phase_limits(band: &str) -> (f64, f64) {
    PHASE_LIMITS[STEREO_BANDS.iter().position(|x| x.0 == band).unwrap_or(0)]
}
/// The correlation a band's imager move narrows a phase band to.
const PHASE_TARGET: f64 = 0.2;
/// The whole master is too wide under this correlation or past this mono fold-down (LU): about one master in twenty
/// of the corpus.
const TOO_WIDE_CORRELATION: f64 = 0.0;
const TOO_WIDE_FOLD_DOWN_LU: f64 = -3.5;

/// A low end whose side sits this far under its mid is narrow enough whatever the reference's (dB).
const LOW_SIDE_FLOOR_DB: f64 = -10.0;
/// The widest a low end is let be without a warning (side over mid, dB): a correlation of about 0.1, where the bass
/// loses over 2 dB when the master is played in mono. One cinematic and one acoustic master in twenty go further.
const LOW_SIDE_CAP_DB: f64 = -1.0;

/// The width that brings a band's correlation from `c` up to `to`: the side scaled by w moves side over mid by
/// w squared, kept within 0.5 to 0.95 for a move.
fn narrow_to(c: f64, to: f64) -> f64 {
    let r = (1.0 - c) / (1.0 + c).max(1e-3);
    let w = ((1.0 - to) / ((1.0 + to) * r)).sqrt();
    r2(w.clamp(0.5, 0.95))
}

fn stereo(spec: &StereoSpectrum, reference: Option<&StereoSpectrum>, style: TonalStyle, prof: &Profile, f: &mut Findings) -> MasterStereo {
    let bands = stereo_bands(spec);
    let m = &prof.mono;
    // phase: the band that matters furthest under its warning limit
    let limits = |b: &StereoRegion| phase_limits(b.band);
    let worst = bands
        .iter()
        .filter(|b| b.level_db.is_some_and(|l| l >= -30.0) && b.correlation.is_some_and(|c| c < limits(b).0))
        .min_by(|a, b| (a.correlation.unwrap_or(1.0) - limits(a).0).total_cmp(&(b.correlation.unwrap_or(1.0) - limits(b).0)));
    let mut phase_band = None;
    if let Some(b) = worst {
        let c = b.correlation.unwrap_or(0.0);
        let sev = if c < limits(b).1 { Sev::Problem } else { Sev::Warn };
        let points = if sev == Sev::Problem { 15.0 } else { 6.0 };
        let mv = if b.band == "low" {
            MasterMove::MonoBelow { hz: 150.0 }
        } else {
            MasterMove::Imager { hz_lo: b.lo_hz, hz_hi: b.hi_hz, width: narrow_to(c, PHASE_TARGET) }
        };
        f.cost(
            sev,
            "phase",
            points,
            format!(
                "The {} band ({}-{} Hz) has a left/right correlation of {c:.2}{}: in mono it cancels and gets thin or disappears{}",
                b.band.replace('_', "-"),
                hz_text(b.lo_hz),
                hz_text(b.hi_hz),
                m.fold_down_lu.map(|d| format!(" (the mono fold-down is {d:+.1} LU)")).unwrap_or_default(),
                if c < -0.8 { ". Near -1 a channel's polarity is flipped: find the track or insert that inverts it (narrowing only softens it)" } else { "" }
            ),
            Some(mv),
        );
        phase_band = Some(b.band);
    }
    let low_matters = m.low_end_db.is_some_and(|l| l > LOW_END_MATTERS_DB);
    // the reference's own low end, else what most of the style's masters stay under
    let (note, warn, against) = match reference.map(|r| side_mid(r, 0.0, LOW_END_HZ)) {
        Some(Some(r)) => (f64::INFINITY, (r + REFERENCE_WIDTH_TOLERANCE_DB).max(LOW_SIDE_FLOOR_DB), format!("the reference's is {:.1} dB under", -r)),
        _ => {
            let [n, w] = style.measured().low_side_db;
            let against = if w >= LOW_SIDE_CAP_DB {
                format!("it needs to be at least {:.0} dB under, or the bass loses over 2 dB in mono", -LOW_SIDE_CAP_DB)
            } else {
                format!("19 in 20 of the {} masters keep it {:.1} dB or more under", style.name(), -w)
            };
            (n, w, against)
        }
    };
    if phase_band != Some("low")
        && low_matters
        && let Some(s) = m.low_end_side_db
        && (s > note || s > warn)
    {
        let (sev, points, hz) = if s > warn { (Sev::Warn, 6.0, 120.0) } else { (Sev::Info, 2.0, 100.0) };
        f.cost(
            sev,
            "low_end_wide",
            points,
            format!(
                "The low end under {LOW_END_HZ:.0} Hz has its side {:.1} dB under its mid ({against}): a wide low end wanders between speakers, loses punch in mono and wastes headroom",
                -s
            ),
            Some(MasterMove::MonoBelow { hz }),
        );
    }
    let ref_bands = reference.map(stereo_bands);
    let wide_bands = band_widths(&bands, ref_bands.as_deref(), style, phase_band, f);
    let too_wide = m.correlation.is_some_and(|c| c < TOO_WIDE_CORRELATION) || m.fold_down_lu.is_some_and(|d| d < TOO_WIDE_FOLD_DOWN_LU);
    if too_wide && phase_band.is_none() {
        // the bands that are too wide have imager moves of their own
        let (then, mv) = if wide_bands { (": the band_wide moves narrow it where it's widest", None) } else { ("", Some(MasterMove::Width { width: 0.85 })) };
        f.cost(
            Sev::Warn,
            "too_wide",
            5.0,
            format!(
                "The master is very wide (correlation {}, mono fold-down {} LU): on phones and in mono parts drop out{then}",
                m.correlation.map(|c| format!("{c:.2}")).unwrap_or_else(|| "-".into()),
                m.fold_down_lu.map(|d| format!("{d:+.1}")).unwrap_or_else(|| "-".into())
            ),
            mv,
        );
    }
    if m.correlation.is_some_and(|c| c > 0.98) {
        f.add(Sev::Info, "narrow", 0.0, "The master is nearly mono: fine for phones, but pan or widen parts in the mix if it should feel wide".into(), None);
    }
    MasterStereo {
        correlation: m.correlation.map(r2),
        fold_down_lu: m.fold_down_lu.map(r1),
        negative_share: r2(m.negative_share),
        low_end_side_db: m.low_end_side_db.map(r1),
        low_end_db: m.low_end_db.map(r1),
        bands,
        phase_problem: phase_band.is_some(),
    }
}

fn dynamics(a: AudioRef, o: &MasterOptions, s: &LoudnessStats, prof: &Profile, series: &Series, f: &mut Findings) -> MasterDynamics {
    let d = &prof.dynamics;
    let (ceiling, tp) = (o.ceiling_dbtp, s.true_peak_dbtp);
    let plr_verdict = match s.plr_db {
        None => "unknown",
        Some(p) if p < 6.0 => "crushed",
        Some(p) if p < 8.0 => "over_limited",
        _ => "ok",
    };
    if let (Some(plr), Some(lufs)) = (s.plr_db, s.integrated_lufs)
        && plr < 8.0
    {
        let sev = if plr < 6.0 { Sev::Problem } else { Sev::Warn };
        let points = (4.0 * (8.0 - plr)).min(20.0);
        if tp >= ceiling - 1.0 {
            let target = r1(lufs - (8.0 - plr).clamp(0.5, 2.0));
            f.cost(
                sev,
                plr_verdict,
                points,
                format!("PLR {plr:.1} dB: the limiter is pushing the master so hard that its peaks are flattened and the attacks lose their punch"),
                Some(MasterMove::Loudness { target_lufs: target }),
            );
        } else {
            f.cost(
                sev,
                plr_verdict,
                points,
                format!("PLR {plr:.1} dB with the true peak {tp:.1} dBTP, under the ceiling: the squashing happens before the limiter; ease the master's glue compressor or the tracks' compressors"),
                None,
            );
        }
    }
    let lra_limit = if o.looped { 10.0 } else { 15.0 };
    let lra_verdict = match s.lra_lu {
        None => "unknown",
        Some(l) if l < 1.5 => "flat",
        Some(l) if l > lra_limit => "wide",
        _ => "ok",
    };
    if let Some(lra) = s.lra_lu {
        if lra > lra_limit {
            let (sev, points) = if o.looped { (Sev::Warn, (2.0 * (lra - lra_limit)).min(10.0)) } else { (Sev::Info, 0.0) };
            f.cost(
                sev,
                "lra_wide",
                points,
                format!("Loudness range {lra:.1} LU (over {lra_limit:.0}): quiet passages vanish under the game's sound effects and loud ones jump out; even out the sections' levels"),
                None,
            );
        } else if lra < 1.5 {
            f.add(Sev::Info, "lra_flat", 0.0, format!("Loudness range {lra:.1} LU: the level never moves; sections that build and release keep a loop from tiring"), None);
        }
    }
    let squashed = d.transient_contrast_db.is_some_and(|c| c < 4.0) && d.onsets_per_s >= 1.0 && s.plr_db.is_some_and(|p| p < 10.0);
    if squashed {
        let mv = (tp >= ceiling - 0.5).then_some(MasterMove::Limiter { release_ms: None, clip_db: Some(1.5) });
        f.cost(
            Sev::Warn,
            "squashed_transients",
            5.0,
            format!(
                "Attacks stand only {:.1} dB out of what precedes them (PLR {:.1} dB): drums and plucks sound flattened; {}",
                d.transient_contrast_db.unwrap_or(0.0),
                s.plr_db.unwrap_or(0.0),
                if mv.is_some() {
                    "the limiter turns each attack down: its clip stage can take the sharpest few samples instead, which keeps the attack's shape"
                } else {
                    "the limiter isn't the cause (the peaks stay under the ceiling): ease the glue and the drums' compressors, or their attack times"
                }
            ),
            mv,
        );
    }
    if let (Some(st), Some(l)) = (s.short_term_max_lufs, s.integrated_lufs)
        && st - l > 8.0
    {
        f.cost(Sev::Warn, "loud_passage", 3.0, format!("The loudest 3 s reach {st:.1} LUFS, {:.1} LU over the whole: that passage will jump out in the game", st - l), None);
    }
    let (psr_min, psr_at, _) = psr(series, o.looped, a.sr);
    if let Some(p) = psr_min
        && p < 5.0
    {
        f.cost(
            Sev::Warn,
            "dense_passage",
            3.0,
            format!("Around {:.1} s the true peak is only {p:.1} dB over the short-term loudness: that passage is limited hard; thin its arrangement or lower it before the master", psr_at.unwrap_or(0.0)),
            None,
        );
    }
    let sections = section_loudness(a, o, s, series);
    for sec in &sections {
        if let Some(v) = sec.vs_song_lu
            && v.abs() > 6.0
        {
            let how = if v > 0.0 { "louder" } else { "quieter" };
            f.add(Sev::Info, "section_level", 0.0, format!("Section '{}' is {:.1} LU {how} than the whole: check it's meant to be", sec.name, v.abs()), None);
        }
    }
    MasterDynamics {
        plr_db: s.plr_db.map(r1),
        plr_verdict,
        lra_lu: s.lra_lu.map(r1),
        lra_verdict,
        onsets_per_s: r2(d.onsets_per_s),
        transient_contrast_db: d.transient_contrast_db.map(r1),
        transient_tail_db: d.transient_tail_db.map(r1),
        squashed_transients: squashed,
        sections,
    }
}

fn section_loudness(a: AudioRef, o: &MasterOptions, s: &LoudnessStats, series: &Series) -> Vec<SectionLoudness> {
    let sr = a.sr as f64;
    o.sections
        .par_iter()
        .map(|(name, start, end)| {
            let end = (*end).min(a.len());
            let lufs = loudness_stats(a.slice(*start, end), false).ok().and_then(|x| x.integrated_lufs);
            let st: Vec<f64> = (0..series.short_term.len())
                .filter(|&i| i * series.hop >= *start && (i + 1) * series.hop <= end)
                .map(|i| series.short_term[i])
                .filter(|v| v.is_finite() && *v > -70.0)
                .collect();
            SectionLoudness {
                name: name.clone(),
                start_s: r2(*start as f64 / sr),
                end_s: r2(end as f64 / sr),
                lufs: lufs.map(r1),
                short_term_max_lufs: st.iter().copied().reduce(f64::max).map(r1),
                short_term_median_lufs: (!st.is_empty()).then(|| r1(median(&st))),
                vs_song_lu: lufs.zip(s.integrated_lufs).map(|(l, m)| r1(l - m)),
            }
        })
        .collect()
}

fn layer_report(a: AudioRef, layers: &[(String, AudioRef)], looped: bool, master_lufs: f64, ceiling: f64) -> MasterLayers {
    let n = a.len();
    let errors: Vec<Option<String>> = layers
        .iter()
        .map(|(_, l)| {
            if l.sr != a.sr {
                Some(format!("its sample rate is {} Hz, the master's {} Hz", l.sr, a.sr))
            } else if l.len() != n {
                Some(format!("it is {} samples long, the master {n}", l.len()))
            } else {
                None
            }
        })
        .collect();
    let (single, (stacked, sum_error)) = rayon::join(
        || layers.par_iter().map(|(_, l)| if l.is_empty() { None } else { loudness_stats(*l, looped).ok() }).collect::<Vec<_>>(),
        || {
            let (mut sl, mut sr) = (vec![0f32; n], vec![0f32; n]);
            let mut out: Vec<Option<LoudnessStats>> = vec![None; layers.len()];
            for (i, (_, l)) in layers.iter().enumerate() {
                if errors[i].is_some() {
                    continue;
                }
                for j in 0..n {
                    sl[j] += l.left[j];
                    sr[j] += l.right[j];
                }
                out[i] = loudness_stats(AudioRef { sr: a.sr, left: &sl, right: &sr }, looped).ok();
            }
            let all = errors.iter().all(|e| e.is_none());
            let err = all.then(|| {
                let e = (0..n).map(|j| (sl[j] as f64 - a.left[j] as f64).abs().max((sr[j] as f64 - a.right[j] as f64).abs())).fold(0.0, f64::max);
                r1(db20(e))
            });
            (out, err)
        },
    );
    let mut prev: Option<f64> = None;
    let many = layers.len() > 1;
    let levels = layers
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let lufs = single[i].as_ref().and_then(|s| s.integrated_lufs);
            let tp = single[i].as_ref().map(|s| s.true_peak_dbtp);
            let st = stacked[i].as_ref();
            let st_lufs = st.and_then(|s| s.integrated_lufs);
            let adds = prev.zip(st_lufs).map(|(p, c)| r1(c - p));
            if st.is_some() {
                prev = st_lufs;
            }
            LayerLevel {
                name: name.clone(),
                lufs: lufs.map(r1),
                true_peak_dbtp: tp.map(r2),
                stacked_lufs: st_lufs.map(r1),
                adds_lu: adds,
                stacked_true_peak_dbtp: st.map(|s| r2(s.true_peak_dbtp)),
                over_ceiling: tp.is_some_and(|t| t > ceiling + 0.05),
                silent: errors[i].is_none() && lufs.is_none_or(|l| l < -40.0),
                dominates: many && lufs.is_some_and(|l| l >= master_lufs - 0.5),
                error: errors[i].clone(),
            }
        })
        .collect();
    MasterLayers { layers: levels, sum_error_dbfs: sum_error, sums_to_master: sum_error.map(|e| e <= -60.0) }
}

fn layer_issues(l: &MasterLayers, master_lufs: f64, ceiling: f64, f: &mut Findings) {
    let mut points = 0.0;
    let mut stack_reported = false;
    let last = l.layers.len().saturating_sub(1);
    for (i, x) in l.layers.iter().enumerate() {
        let name = &x.name;
        if let Some(e) = &x.error {
            points += 3.0;
            f.add(Sev::Warn, "layer_unusable", 3.0, format!("Layer '{name}' can't be checked: {e}; export the layers again together with the master"), None);
            continue;
        }
        if x.silent {
            points += 3.0;
            f.add(Sev::Warn, "layer_silent", 3.0, format!("Layer '{name}' is silent (under -40 LUFS): remove it, or check that its tracks play"), None);
            continue;
        }
        if let Some(tp) = x.true_peak_dbtp
            && x.over_ceiling
        {
            let (sev, p) = if tp > 0.0 { (Sev::Problem, 5.0) } else { (Sev::Warn, 3.0) };
            points += p;
            f.add(sev, "layer_over_ceiling", p, format!("Layer '{name}' peaks at {tp:+.1} dBTP on its own, over the {ceiling:.1} dBTP ceiling: it can clip when the game plays it without the others"), None);
        }
        if i < last
            && !stack_reported
            && let Some(tp) = x.stacked_true_peak_dbtp
            && tp > ceiling + 0.05
        {
            stack_reported = true;
            points += 3.0;
            f.add(Sev::Warn, "layer_stack_over_ceiling", 3.0, format!("The layers up to '{name}' peak at {tp:+.1} dBTP together, over the ceiling: the game's mix of just those layers can clip"), None);
        }
        if x.dominates {
            f.add(
                Sev::Info,
                "layer_dominates",
                0.0,
                format!(
                    "Layer '{name}' alone is {} LUFS, within 0.5 LU of the whole master ({master_lufs:.1} LUFS): the other layers add little loudness, so fading them in or out will barely be heard",
                    x.lufs.map(|v| format!("{v:.1}")).unwrap_or_default()
                ),
                None,
            );
        }
    }
    if let Some(e) = l.sum_error_dbfs
        && e > -60.0
    {
        points += 10.0;
        f.add(Sev::Problem, "layer_sum", 10.0, format!("The layers sum to the master only within {e:.1} dBFS (under -60 expected): a layer is missing or out of date; export the layers again"), None);
    }
    f.penalty += points.min(20.0);
}

fn seam_issues(s: &SeamReport, f: &mut Findings) {
    if !s.jump_ok {
        f.cost(
            Sev::Problem,
            "seam_jump",
            10.0,
            format!(
                "The loop wraps with a step of {:.4} / {:.4} (L/R), larger than 99.9 % of the track's own steps ({:.4} / {:.4}) and than the music moves around the seam ({:.4} / {:.4}): it clicks every time it loops",
                s.jump[0], s.jump[1], s.p999_step[0], s.p999_step[1], s.local_step[0], s.local_step[1]
            ),
            None,
        );
    }
    // a seam is a bar line too: only clearly more abrupt than the song's own bar lines or attacks is suspect
    let worse = |v: f64, inner: Option<f64>, floor: f64| v > floor && inner.is_none_or(|b| v > b + 6.0);
    if worse(s.hf_click_db, s.reference_hf_max_db, 12.0) {
        f.cost(Sev::Warn, "seam_click", 5.0, format!("A high-frequency spike of {:.1} dB at the loop seam: listen to the wrap; a cut note or an effect tail usually causes it", s.hf_click_db), None);
    }
    if worse(s.level_step_db.abs(), s.reference_level_step_max_db, 6.0) {
        f.cost(Sev::Warn, "seam_level_step", 3.0, format!("The level changes by {:+.1} dB across the loop seam: the end and the start don't match", s.level_step_db), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcheq::tests::engine_eq;
    use crate::num::irfft;
    use crate::profile::tests::noise;

    const SR: u32 = 48000;

    /// `x` (length a power of two) through a zero-phase filter of magnitude `gain(f)`.
    fn shaped(x: &[f32], gain: impl Fn(f64) -> f64) -> Vec<f32> {
        let n = x.len();
        let mut s = rfft(&x.iter().map(|&v| v as f64).collect::<Vec<_>>(), n);
        for (k, c) in s.iter_mut().enumerate() {
            *c *= gain(k as f64 * SR as f64 / n as f64);
        }
        irfft(&s, n).iter().map(|&v| v as f32).collect()
    }

    /// About 22 s of noise whose 1/3-octave balance follows `style`'s target.
    fn on_target(style: TonalStyle, seed: u32) -> Vec<f32> {
        shaped(&noise(1 << 20, seed), |f| if f < 15.0 { 0.0 } else { f.powf(-0.5) * 10f64.powf(target_db(style, f) / 20.0) })
    }

    fn band(x: &[f32], lo: f64, hi: f64) -> Vec<f32> {
        shaped(x, |f| if f >= lo && f <= hi { 1.0 } else { 0.0 })
    }

    fn gain(x: &[f32], db: f64) -> Vec<f32> {
        let g = 10f64.powf(db / 20.0) as f32;
        x.iter().map(|v| v * g).collect()
    }

    /// `l`/`r` scaled together to `lufs`.
    fn at_lufs(l: &[f32], r: &[f32], lufs: f64) -> (Vec<f32>, Vec<f32>) {
        let s = loudness_stats(AudioRef::new(SR, l, r).unwrap(), false).unwrap();
        let d = lufs - s.integrated_lufs.unwrap();
        (gain(l, d), gain(r, d))
    }

    fn check(l: &[f32], r: &[f32], o: &MasterOptions) -> MasterReport {
        check_master(AudioRef::new(SR, l, r).unwrap(), o).unwrap()
    }

    fn issue<'a>(r: &'a MasterReport, code: &str) -> Option<&'a MasterIssue> {
        r.issues.iter().find(|i| i.code == code)
    }

    fn region<'a>(r: &'a MasterReport, name: &str) -> &'a TonalRegion {
        r.tonal.as_ref().unwrap().regions.iter().find(|x| x.region == name).unwrap()
    }

    /// A target-shaped stereo master: mono below 300 Hz, a little width above.
    fn good_master(style: TonalStyle, lufs: f64) -> (Vec<f32>, Vec<f32>) {
        let l = on_target(style, 3);
        let side = gain(&band(&on_target(style, 4), 300.0, 24000.0), -10.0);
        let r: Vec<f32> = l.iter().zip(&side).map(|(a, b)| a + b).collect();
        let l: Vec<f32> = l.iter().zip(&side).map(|(a, b)| a - b).collect();
        at_lufs(&l, &r, lufs)
    }

    #[test]
    fn a_master_on_target_scores_high() {
        let o = MasterOptions { target_lufs: -18.0, ..MasterOptions::default() };
        for style in [TonalStyle::Balanced, TonalStyle::Electronic] {
            let (l, r) = good_master(style, -18.0);
            let rep = check(&l, &r, &MasterOptions { style, ..o.clone() });
            assert!(rep.score >= 95, "{style:?}: {} {:#?}", rep.summary, rep.issues);
            assert!(rep.issues.iter().all(|i| i.severity == "info"), "{style:?}: {:#?}", rep.issues);
            let t = rep.tonal.as_ref().unwrap();
            assert!(t.deviation_rms_db < 1.0 && t.correction.is_empty(), "{style:?}: {t:#?}");
            assert!(t.regions.iter().all(|x| x.verdict == "ok"), "{:#?}", t.regions);
            assert_eq!(t.target, style.name());
            assert!(rep.summary.contains("no problems") && rep.summary.contains("-18.0 LUFS"), "{}", rep.summary);
        }
        // the other style's target: the same master is off, and the report says which style it reads as
        let (l, r) = good_master(TonalStyle::Acoustic, -18.0);
        let rep = check(&l, &r, &MasterOptions { style: TonalStyle::Electronic, ..o.clone() });
        let t = rep.tonal.as_ref().unwrap();
        assert!(t.deviation_rms_db > 1.0, "{t:#?}");
        assert_eq!((t.styles.len(), t.closest_style), (4, Some(TonalStyle::Acoustic)), "{:#?}", t.styles);
        let hint = issue(&rep, "closer_style").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert!(hint.severity == "info" && hint.message.contains("style acoustic"), "{}", hint.message);
        // checked as what it is: no hint
        let rep = check(&l, &r, &MasterOptions { style: TonalStyle::Acoustic, ..o });
        assert!(issue(&rep, "closer_style").is_none(), "{:#?}", rep.issues);
    }

    #[test]
    fn a_low_mid_bump_is_muddy_and_the_suggested_cut_fixes_it() {
        // balanced: its masters agree most in the low mids
        let o = MasterOptions { target_lufs: -18.0, style: TonalStyle::Balanced, ..MasterOptions::default() };
        let (l, r) = good_master(TonalStyle::Balanced, -18.0);
        let bump = [EqBand { shape: EqShape::Peak, hz: 300.0, db: 12.0, q: 1.0 }];
        let (l, r) = (engine_eq(&l, SR as f64, &bump), engine_eq(&r, SR as f64, &bump));
        let rep = check(&l, &r, &o);
        let mud = issue(&rep, "muddy").unwrap_or_else(|| panic!("{:#?} {:#?}", rep.issues, rep.tonal.as_ref().unwrap().regions));
        let Some(MasterMove::Eq { shape, hz, db, q, .. }) = mud.suggestion else { panic!("{mud:?}") };
        assert!(shape == EqShape::Peak && (200.0..=450.0).contains(&hz) && (-3.0..-1.0).contains(&db) && (0.7..=1.4).contains(&q), "{mud:?}");
        assert!(mud.message.contains("Try:"), "{}", mud.message);
        assert!(rep.score < 95, "{}", rep.summary);
        // applying the correction moves the region most of the way back
        let before = region(&rep, "mud").deviation_db.unwrap();
        let fix = rep.tonal.as_ref().unwrap().correction.clone();
        assert!(fix.iter().all(|b| b.db.abs() <= 3.0 && (0.5..=1.4).contains(&b.q)), "{fix:?}");
        let (l2, r2) = (engine_eq(&l, SR as f64, &fix), engine_eq(&r, SR as f64, &fix));
        let again = check(&l2, &r2, &o);
        let after = region(&again, "mud").deviation_db.unwrap();
        assert!(after < before - 2.0, "{before} -> {after}");
        assert!(again.score > rep.score, "{} -> {}", rep.summary, again.summary);
    }

    #[test]
    fn every_region_out_of_balance_gets_a_move_and_a_far_one_points_to_the_mix() {
        let o = MasterOptions { target_lufs: -18.0, style: TonalStyle::Cinematic, ..MasterOptions::default() };
        let (l, r) = good_master(TonalStyle::Cinematic, -18.0);
        let eq = [
            EqBand { shape: EqShape::Peak, hz: 65.0, db: -24.0, q: 1.0 },
            EqBand { shape: EqShape::Peak, hz: 700.0, db: 18.0, q: 1.2 },
            EqBand { shape: EqShape::Peak, hz: 3200.0, db: -14.0, q: 1.2 },
            EqBand { shape: EqShape::Peak, hz: 7000.0, db: 16.0, q: 1.2 },
            EqBand { shape: EqShape::HighShelf, hz: 11000.0, db: -14.0, q: 0.7 },
        ];
        let (l, r) = (engine_eq(&l, SR as f64, &eq), engine_eq(&r, SR as f64, &eq));
        let rep = check(&l, &r, &o);
        let tonal: Vec<&MasterIssue> =
            rep.issues.iter().filter(|i| REGIONS.iter().any(|x| x.3 == i.code || x.4 == i.code) && i.code != "sub_light").collect();
        assert!(tonal.len() >= 3, "{:#?} {:#?}", rep.issues, rep.tonal.as_ref().unwrap().regions);
        for i in &tonal {
            let Some(MasterMove::Eq { db, q, .. }) = i.suggestion else { panic!("{i:?}") };
            assert!(db.abs() <= 3.0 && (0.7..=1.4).contains(&q), "{i:?}");
        }
        let boxy = issue(&rep, "boxy").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert!(region_out(&rep, "boxy") > FIX_IN_MIX_DB && boxy.message.contains("mix_check"), "{}", boxy.message);
        let near: Vec<_> = tonal.iter().filter(|i| region_out(&rep, &i.code) <= FIX_IN_MIX_DB).collect();
        assert!(!near.is_empty() && near.iter().all(|i| !i.message.contains("mix_check")), "{near:#?}");
    }

    #[test]
    fn a_region_the_fitted_eq_leaves_out_still_gets_one_band() {
        // too little bass: a boost across 40-100 Hz, as far as a move goes
        let Some(MasterMove::Eq { shape, hz, db, q, .. }) = region_eq(40.0, 100.0, -9.0, 3.3) else { panic!() };
        assert!(shape == EqShape::Peak && (60.0..=66.0).contains(&hz) && db == 3.0 && (0.7..=1.4).contains(&q), "{hz} {db} {q}");
        // just outside: back to within half the tolerance, no further
        let Some(MasterMove::Eq { db, .. }) = region_eq(500.0, 1000.0, 3.0, 2.5) else { panic!() };
        assert_eq!(db, -1.8);
        // the ends of the spectrum get shelves; a sub that isn't there gets nothing
        assert!(matches!(region_eq(10000.0, 20000.0, -6.0, 4.1), Some(MasterMove::Eq { shape: EqShape::HighShelf, db: 3.0, .. })));
        assert!(matches!(region_eq(20.0, 40.0, 7.0, 4.3), Some(MasterMove::Eq { shape: EqShape::LowShelf, db: -3.0, .. })));
        assert_eq!(region_eq(20.0, 40.0, -12.0, 4.3), None);
    }

    /// How far past its tolerance the region behind an issue code is (dB).
    fn region_out(rep: &MasterReport, code: &str) -> f64 {
        let ri = REGIONS.iter().position(|x| x.3 == code || x.4 == code).unwrap();
        let x = region(rep, REGIONS[ri].0);
        x.deviation_db.unwrap().abs() - x.tolerance_db
    }

    #[test]
    fn a_crushed_master_is_told_to_back_off() {
        let (l, r) = good_master(TonalStyle::Balanced, -18.0);
        let clip = |x: &[f32]| gain(x, 18.0).iter().map(|v| v.clamp(-0.85, 0.85)).collect::<Vec<f32>>();
        let (l, r) = (clip(&l), clip(&r));
        let rep = check(&l, &r, &MasterOptions::default());
        assert_eq!(rep.dynamics.plr_verdict, "crushed", "{:#?}", rep.loudness);
        let i = issue(&rep, "crushed").unwrap();
        let Some(MasterMove::Loudness { target_lufs }) = i.suggestion else { panic!("{i:?}") };
        assert!(target_lufs < rep.loudness.integrated_lufs.unwrap(), "{i:?}");
        let t = issue(&rep, "loudness_target").unwrap();
        assert_eq!((t.severity, &t.suggestion), ("problem", &Some(MasterMove::Loudness { target_lufs: -15.0 })));
        assert!(rep.score < 60 && rep.issues[0].severity == "problem", "{}", rep.summary);
        assert!(rep.summary.contains("problems"), "{}", rep.summary);
    }

    #[test]
    fn an_out_of_phase_low_end_is_made_mono() {
        let x = on_target(TonalStyle::Balanced, 5);
        let (low, rest) = (band(&x, 20.0, 140.0), band(&x, 160.0, 24000.0));
        let l: Vec<f32> = low.iter().zip(&rest).map(|(a, b)| a + b).collect();
        let r: Vec<f32> = low.iter().zip(&rest).map(|(a, b)| b - a).collect();
        let (l, r) = at_lufs(&l, &r, -18.0);
        let rep = check(&l, &r, &MasterOptions { target_lufs: -18.0, ..MasterOptions::default() });
        let p = issue(&rep, "phase").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert_eq!((p.severity, &p.suggestion), ("problem", &Some(MasterMove::MonoBelow { hz: 150.0 })));
        assert!(rep.stereo.phase_problem && rep.stereo.bands[0].correlation.unwrap() < -0.9, "{:#?}", rep.stereo);
        assert!(issue(&rep, "low_end_wide").is_none() && issue(&rep, "too_wide").is_none(), "{:#?}", rep.issues);
        // a wide but in-phase low end only needs narrowing
        let side = gain(&band(&on_target(TonalStyle::Balanced, 9), 30.0, 110.0), -8.0);
        let l: Vec<f32> = x.iter().zip(&side).map(|(a, b)| a + b).collect();
        let r: Vec<f32> = x.iter().zip(&side).map(|(a, b)| a - b).collect();
        let (l, r) = at_lufs(&l, &r, -18.0);
        let rep = check(&l, &r, &MasterOptions { target_lufs: -18.0, ..MasterOptions::default() });
        assert!(rep.stereo.low_end_side_db.unwrap() > -16.0, "{:#?}", rep.stereo);
        let w = issue(&rep, "low_end_wide").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert!(matches!(w.suggestion, Some(MasterMove::MonoBelow { .. })), "{w:?}");
    }

    #[test]
    fn layers_are_stacked_and_summed() {
        // a full layer and a quiet one 15 LU under it
        let a = on_target(TonalStyle::Balanced, 6);
        let a = at_lufs(&a, &a, -15.2).0;
        let b = band(&noise(1 << 20, 7), 2000.0, 4000.0);
        let b = at_lufs(&b, &b, -30.0).0;
        let sum = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x + y).collect::<Vec<f32>>();
        let m = sum(&a, &b);
        let d = -15.0 - loudness_stats(AudioRef::new(SR, &m, &m).unwrap(), false).unwrap().integrated_lufs.unwrap();
        let (a, b) = (gain(&a, d), gain(&b, d));
        let c = vec![0f32; a.len()];
        let m = sum(&a, &b);
        fn view(x: &[f32]) -> AudioRef<'_> {
            AudioRef::new(SR, x, x).unwrap()
        }
        let o = MasterOptions { layers: vec![("a".into(), view(&a)), ("b".into(), view(&b)), ("c".into(), view(&c))], ..MasterOptions::default() };
        let rep = check(&m, &m, &o);
        let ls = rep.layers.as_ref().unwrap();
        assert_eq!(ls.sums_to_master, Some(true), "{ls:#?}");
        assert!(ls.layers[0].dominates && !ls.layers[1].dominates && ls.layers[2].silent, "{ls:#?}");
        assert!(ls.layers[0].adds_lu.is_none() && ls.layers[1].adds_lu.unwrap() < 0.5, "{ls:#?}");
        assert!(issue(&rep, "layer_silent").is_some() && issue(&rep, "layer_dominates").is_some(), "{:#?}", rep.issues);
        assert!(issue(&rep, "layer_sum").is_none());
        // a layer missing from the export, and one of the wrong length
        let short = vec![0.1f32; 1000];
        let o = MasterOptions { layers: vec![("a".into(), view(&a))], ..MasterOptions::default() };
        let rep = check(&m, &m, &o);
        assert_eq!(rep.layers.as_ref().unwrap().sums_to_master, Some(false));
        assert_eq!(issue(&rep, "layer_sum").unwrap().severity, "problem");
        let o = MasterOptions { layers: vec![("a".into(), view(&a)), ("short".into(), view(&short))], ..MasterOptions::default() };
        let rep = check(&m, &m, &o);
        assert!(issue(&rep, "layer_unusable").unwrap().message.contains("1000 samples"), "{:#?}", rep.issues);
        assert_eq!(rep.layers.as_ref().unwrap().sum_error_dbfs, None);
    }

    #[test]
    fn sibilance_bursts_get_a_de_esser() {
        let (mut l, mut r) = good_master(TonalStyle::Balanced, -20.0);
        let hiss = band(&noise(1 << 20, 8), 5000.0, 9000.0);
        let (len, period) = (SR as usize * 40 / 1000, SR as usize / 2);
        for (i, h) in hiss.iter().enumerate() {
            let p = i % period;
            if p < len {
                let w = (std::f64::consts::PI * p as f64 / len as f64).sin() as f32;
                l[i] += h * w * 1.2;
                r[i] += h * w * 1.2;
            }
        }
        let rep = check(&l, &r, &MasterOptions { target_lufs: -18.0, ..MasterOptions::default() });
        let s = rep.tonal.as_ref().unwrap().sibilance.clone().unwrap();
        assert!(s.bursty, "{s:?}");
        let de = rep.issues.iter().find_map(|i| match i.suggestion {
            Some(MasterMove::DeEss { hz, range_db }) => Some((hz, range_db)),
            _ => None,
        });
        let (hz, range) = de.unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert!((4000.0..=7500.0).contains(&hz) && (-6.0..=-2.0).contains(&range), "{hz} {range}");
        // steady noise has no bursts
        let (l, r) = good_master(TonalStyle::Balanced, -20.0);
        let rep = check(&l, &r, &MasterOptions { target_lufs: -20.0, ..MasterOptions::default() });
        assert!(!rep.tonal.unwrap().sibilance.unwrap().bursty);
    }

    #[test]
    fn a_reference_becomes_the_target() {
        let x = gain(&on_target(TonalStyle::Balanced, 11), 20.0);
        let dark = engine_eq(&x, SR as f64, &[EqBand { shape: EqShape::HighShelf, hz: 5000.0, db: -6.0, q: 0.7 }]);
        let o = MasterOptions { target_lufs: -18.0, reference: Some(AudioRef::new(SR, &x, &x).unwrap()), ..MasterOptions::default() };
        let (l, r) = at_lufs(&dark, &dark, -18.0);
        let rep = check(&l, &r, &o);
        let t = rep.tonal.as_ref().unwrap();
        assert_eq!(t.target, "reference");
        let dull = issue(&rep, "dull").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        let Some(MasterMove::Eq { db, hz, .. }) = dull.suggestion else { panic!("{dull:?}") };
        assert!(db > 0.0 && hz >= 3000.0, "{dull:?}");
        assert!(issue(&rep, "lacks_air").is_none(), "merged into dull: {:#?}", rep.issues);
        // the reference against itself: nothing to change
        let rep = check(&x, &x, &MasterOptions { target_lufs: -40.0, ..o.clone() });
        let t = rep.tonal.unwrap();
        assert!(t.deviation_rms_db < 0.01 && t.correction.is_empty(), "{t:#?}");
    }

    #[test]
    fn seams_sections_and_silence() {
        // a sine that stops mid-cycle clicks at the wrap
        let n = SR as usize * 4;
        let x: Vec<f32> = (0..n).map(|i| (0.3 * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / SR as f64 + 0.5).sin()) as f32).collect();
        let half = n / 2;
        let y: Vec<f32> = x.iter().enumerate().map(|(i, v)| if i < half { v * 0.25 } else { *v }).collect();
        let o = MasterOptions {
            looped: true,
            sections: vec![("quiet".into(), 0, half), ("loud".into(), half, n + 100)],
            ..MasterOptions::default()
        };
        let rep = check(&y, &y, &o);
        assert!(rep.seam.is_some() && issue(&rep, "seam_jump").is_some(), "{:#?}", rep.issues);
        let s = &rep.dynamics.sections;
        assert_eq!((s[0].name.as_str(), s[1].end_s), ("quiet", 4.0));
        assert!(s[0].vs_song_lu.unwrap() < -6.0 && s[1].vs_song_lu.unwrap() > 0.0, "{s:#?}");
        assert!(issue(&rep, "section_level").is_some());
        // silence
        let z = vec![0f32; SR as usize];
        let rep = check(&z, &z, &MasterOptions::default());
        assert_eq!((rep.score, rep.issues[0].code.as_str()), (0, "silent"));
        // bad options
        let a = AudioRef::new(SR, &z, &z).unwrap();
        assert!(check_master(a, &MasterOptions { target_lufs: 3.0, ..MasterOptions::default() }).is_err());
        assert!(check_master(a, &MasterOptions { sections: vec![("x".into(), 10, 5)], ..MasterOptions::default() }).is_err());
        assert!(check_master(a.slice(0, 100), &MasterOptions::default()).is_err());
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// `x` scaled to `db` dB of `to`'s RMS.
    fn rms_like(x: &[f32], to: &[f32], db: f64) -> Vec<f32> {
        gain(x, db + 20.0 * (rms(to) / rms(x)).log10())
    }

    /// A master of the same mid as [`good_master`] with `side` as its side signal.
    fn with_side(side: &[f32], lufs: f64) -> (Vec<f32>, Vec<f32>) {
        let x = on_target(TonalStyle::Balanced, 3);
        let l: Vec<f32> = x.iter().zip(side).map(|(a, b)| a - b).collect();
        let r: Vec<f32> = x.iter().zip(side).map(|(a, b)| a + b).collect();
        at_lufs(&l, &r, lufs)
    }

    #[test]
    fn technical_faults_get_a_high_pass_and_a_cut_off_top_the_exciter() {
        let o = MasterOptions { target_lufs: -18.0, ..MasterOptions::default() };
        let (l, r) = good_master(TonalStyle::Balanced, -18.0);
        let rep = check(&l, &r, &o);
        assert!(rep.technical.dc_offset_dbfs < -80.0 && rep.technical.subsonic_db.unwrap() < -30.0, "{:?}", rep.technical);
        assert_eq!(rep.technical.band_limit_hz, None);
        assert!(["dc_offset", "subsonic", "band_limited"].iter().all(|c| issue(&rep, c).is_none()), "{:#?}", rep.issues);

        // a DC offset of 0.02 (-34 dBFS) and rumble under 20 Hz 6 dB under the rest
        let rumble = rms_like(&band(&noise(1 << 20, 21), 6.0, 16.0), &l, -6.0);
        let (l2, r2): (Vec<f32>, Vec<f32>) = (l.iter().zip(&rumble).map(|(a, b)| a + b + 0.02).collect(), r.iter().zip(&rumble).map(|(a, b)| a + b + 0.02).collect());
        let rep = check(&l2, &r2, &o);
        assert!((rep.technical.dc_offset_dbfs + 34.0).abs() < 1.0, "{:?}", rep.technical);
        let dc = issue(&rep, "dc_offset").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert_eq!(dc.suggestion, Some(MasterMove::HighPass { hz: 10.0, slope: 12 }));
        let sub = issue(&rep, "subsonic").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert_eq!(sub.suggestion, Some(MasterMove::HighPass { hz: 20.0, slope: 24 }));

        // a brick wall at 11 kHz is cut off, and the missing air gets the exciter rather than an EQ boost
        let wall = |x: &[f32]| shaped(x, |f| if f > 11000.0 { 0.0 } else { 1.0 });
        let rep = check(&wall(&l), &wall(&r), &o);
        let hz = rep.technical.band_limit_hz.unwrap_or_else(|| panic!("{:?}", rep.technical));
        assert!((10800.0..=11200.0).contains(&hz), "{hz}");
        let b = issue(&rep, "band_limited").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        assert_eq!((b.severity, &b.suggestion), ("warn", &Some(MasterMove::Exciter { hz_lo: 2500.0, hz_hi: 8000.0, mix: 0.5, mode: Saturation::Tube })));
        if let Some(air) = issue(&rep, "lacks_air") {
            assert!(air.suggestion.is_none() && air.message.contains("cut off at"), "{air:?}");
        }
        // a dark master that rolls off 36 dB per octave above 3 kHz is not cut off
        let dark = |x: &[f32]| shaped(x, |f| if f > 3000.0 { (f / 3000.0).powf(-6.0) } else { 1.0 });
        let rep = check(&dark(&l), &dark(&r), &o);
        assert_eq!(rep.technical.band_limit_hz, None, "{:#?}", rep.issues);
    }

    #[test]
    fn stereo_bands_against_a_reference_get_imager_moves() {
        let reference = good_master(TonalStyle::Balanced, -18.0);
        let o = MasterOptions { target_lufs: -18.0, reference: Some(AudioRef::new(SR, &reference.0, &reference.1).unwrap()), ..MasterOptions::default() };
        let base = gain(&band(&on_target(TonalStyle::Balanced, 4), 300.0, 24000.0), -10.0);
        // the mid band 6 dB wider than the reference's, the high band 10 dB narrower
        let side: Vec<f32> = base
            .iter()
            .zip(band(&base, 800.0, 5000.0))
            .zip(band(&base, 5000.0, 24000.0))
            .map(|((s, m), h)| s + m * (10f32.powf(6.0 / 20.0) - 1.0) + h * (10f32.powf(-10.0 / 20.0) - 1.0))
            .collect();
        let (l, r) = with_side(&side, -18.0);
        let rep = check(&l, &r, &o);
        let wide = issue(&rep, "band_wide").unwrap_or_else(|| panic!("{:#?} {:#?}", rep.issues, rep.stereo.bands));
        let Some(MasterMove::Imager { hz_lo, hz_hi, width }) = wide.suggestion else { panic!("{wide:?}") };
        assert!((hz_lo, hz_hi) == (800.0, 5000.0) && (0.5..0.8).contains(&width), "{wide:?}");
        let narrow = issue(&rep, "band_narrow").unwrap_or_else(|| panic!("{:#?}", rep.issues));
        let Some(MasterMove::Imager { hz_lo, width, .. }) = narrow.suggestion else { panic!("{narrow:?}") };
        assert!(hz_lo == 5000.0 && width > 1.0, "{narrow:?}");
        assert!(issue(&rep, "phase").is_none(), "{:#?}", rep.issues);
    }

    #[test]
    fn a_phase_band_above_the_bass_is_narrowed_with_the_imager() {
        // 800 Hz to 5 kHz: the side twice the mid, a correlation of -1/3
        let x = on_target(TonalStyle::Balanced, 3);
        let side = rms_like(&band(&on_target(TonalStyle::Balanced, 14), 800.0, 5000.0), &band(&x, 800.0, 5000.0), 3.0);
        let (l, r) = with_side(&side, -18.0);
        let rep = check(&l, &r, &MasterOptions { target_lufs: -18.0, ..MasterOptions::default() });
        let p = issue(&rep, "phase").unwrap_or_else(|| panic!("{:#?} {:#?}", rep.issues, rep.stereo.bands));
        let Some(MasterMove::Imager { hz_lo, width, .. }) = p.suggestion else { panic!("{p:?}") };
        assert!(p.severity == "warn" && hz_lo == 800.0 && (0.5..0.7).contains(&width), "{p:?}");
        assert!((narrow_to(-1.0 / 3.0, PHASE_TARGET) - 0.58).abs() < 0.011 && narrow_to(0.9, 0.2) == 0.95 && narrow_to(-0.99, 0.2) == 0.5);
    }

    #[test]
    fn a_region_loud_only_at_times_gets_a_dynamic_eq_band() {
        // 2-5 kHz 16 dB up for 200 ms of every second
        let (mut l, mut r) = good_master(TonalStyle::Balanced, -20.0);
        let h = gain(&band(&l, 2000.0, 5000.0), 20.0 * (10f64.powf(16.0 / 20.0) - 1.0).log10());
        for (i, v) in h.iter().enumerate() {
            if i % (SR as usize) < SR as usize / 5 {
                l[i] += v;
                r[i] += v;
            }
        }
        let rep = check(&l, &r, &MasterOptions { target_lufs: -18.0, ..MasterOptions::default() });
        let harsh = issue(&rep, "harsh").unwrap_or_else(|| panic!("{:#?} {:#?}", rep.issues, rep.tonal.as_ref().unwrap().regions));
        let Some(MasterMove::DynEq { shape, hz, range_db, .. }) = harsh.suggestion else { panic!("{harsh:?}") };
        assert!(shape == EqShape::Peak && (2500.0..=4000.0).contains(&hz) && (-6.0..=-2.0).contains(&range_db), "{harsh:?}");
        assert!(harsh.message.contains("comes and goes"), "{}", harsh.message);
        assert!(region(&rep, "harsh").variability_db.unwrap() >= VARIABLE_DB[6], "{:?}", region(&rep, "harsh"));
    }

    #[test]
    fn a_region_too_loud_in_the_sides_is_cut_on_the_side() {
        let reference = good_master(TonalStyle::Balanced, -18.0);
        let o = MasterOptions { target_lufs: -18.0, reference: Some(AudioRef::new(SR, &reference.0, &reference.1).unwrap()), ..MasterOptions::default() };
        // the reference's side is 10 dB under its mid; this one's is 2 dB over it from 2 to 5 kHz
        let x = on_target(TonalStyle::Balanced, 3);
        let base = gain(&band(&on_target(TonalStyle::Balanced, 4), 300.0, 24000.0), -10.0);
        let extra = rms_like(&band(&on_target(TonalStyle::Balanced, 13), 2000.0, 5000.0), &band(&x, 2000.0, 5000.0), 2.0);
        let side: Vec<f32> = base.iter().zip(&extra).map(|(a, b)| a + b).collect();
        let (l, r) = with_side(&side, -18.0);
        let rep = check(&l, &r, &o);
        let harsh = issue(&rep, "harsh").unwrap_or_else(|| panic!("{:#?} {:#?}", rep.issues, rep.tonal.as_ref().unwrap().regions));
        let Some(MasterMove::Eq { db, part, .. }) = harsh.suggestion else { panic!("{harsh:?}") };
        assert!(part == Part::Side && (-4.5..-0.5).contains(&db), "{harsh:?}");
        assert!(harsh.message.contains("side signal"), "{}", harsh.message);
        assert!(side_gain(-2.0, 0.0).is_some_and(|g| g < -2.0) && side_gain(-2.0, -30.0).is_none());
    }

    #[test]
    fn targets_moves_and_styles_serialise_plainly() {
        let m = MasterMove::Eq { shape: EqShape::LowShelf, hz: 120.0, db: -2.0, q: 0.7, part: Part::Stereo };
        assert_eq!(serde_json::to_value(&m).unwrap(), serde_json::json!({"kind": "eq", "shape": "low_shelf", "hz": 120.0, "db": -2.0, "q": 0.7}));
        let m = MasterMove::Eq { shape: EqShape::HighShelf, hz: 8000.0, db: -3.0, q: 0.7, part: Part::Side };
        assert_eq!(serde_json::to_value(&m).unwrap()["part"], "side");
        let m = MasterMove::Exciter { hz_lo: 2500.0, hz_hi: 8000.0, mix: 0.5, mode: Saturation::Tape };
        assert_eq!(serde_json::to_value(&m).unwrap(), serde_json::json!({"kind": "exciter", "hz_lo": 2500.0, "hz_hi": 8000.0, "mix": 0.5, "mode": "tape"}));
        let m: MasterMove = serde_json::from_str(r#"{"kind":"dyn_eq","shape":"peak","hz":3000,"q":1,"range_db":-4}"#).unwrap();
        assert_eq!(m, MasterMove::DynEq { shape: EqShape::Peak, hz: 3000.0, q: 1.0, range_db: -4.0 });
        assert_eq!(serde_json::to_value(MasterMove::HighPass { hz: 20.0, slope: 24 }).unwrap()["kind"], "high_pass");
        let m: MasterMove = serde_json::from_str(r#"{"kind":"limiter","release_ms":null,"clip_db":1.5}"#).unwrap();
        assert_eq!(m, MasterMove::Limiter { release_ms: None, clip_db: Some(1.5) });
        assert_eq!(serde_json::to_value(MasterMove::MonoBelow { hz: 120.0 }).unwrap(), serde_json::json!({"kind": "mono_below", "hz": 120.0}));
        assert_eq!("Cinematic".parse::<TonalStyle>().unwrap(), TonalStyle::Cinematic);
        assert!("loud".parse::<TonalStyle>().is_err());
        assert_eq!(serde_json::to_value(TonalStyle::Electronic).unwrap(), "electronic");
        // the measured tables: read at the band centres, in between and past the ends
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        for (i, &fc) in third_octave_centres().iter().enumerate() {
            assert!(near(target_db(TonalStyle::Balanced, fc), BALANCED.median[i]), "{fc}");
        }
        assert!(near(target_db(TonalStyle::Balanced, 10f64.powf(3.05)), (BALANCED.median[16] + BALANCED.median[17]) / 2.0));
        assert!(near(target_db(TonalStyle::Acoustic, 5.0), ACOUSTIC.median[0]) && near(target_db(TonalStyle::Acoustic, 24000.0), ACOUSTIC.median[28]));
        assert!(target_db(TonalStyle::Cinematic, f64::NAN).is_finite());
        // the corridor: 2.5 spreads, narrow through the mids and wide at the ends
        assert!(near(tolerance_db(TonalStyle::Balanced, 1000.0), 8.0) && near(tolerance_db(TonalStyle::Balanced, 25.0), 19.5));
        assert!(near(region_tolerance_db(TonalStyle::Electronic, 5), 5.75));
        for s in TonalStyle::ALL {
            let m = s.measured();
            // what tonal_profile measures: the median over the bands taken off
            let mut v = m.median.to_vec();
            v.sort_by(f64::total_cmp);
            assert!(v[14].abs() < 0.05, "{s:?}");
            assert!(tolerance_db(s, 1000.0) < tolerance_db(s, 30.0) && tolerance_db(s, 1000.0) < tolerance_db(s, 14000.0), "{s:?}");
            assert!(target_db(s, 100.0) > target_db(s, 1000.0) && target_db(s, 1000.0) > target_db(s, 10000.0), "{s:?}");
            assert!(target_db(s, 20.0) < target_db(s, 80.0), "{s:?}");
        }
        assert!(target_db(TonalStyle::Electronic, 30.0) > target_db(TonalStyle::Acoustic, 30.0) + 6.0);
        // the stereo tables
        for s in TonalStyle::ALL {
            let [note, warn] = s.measured().low_side_db;
            assert!(note <= warn && warn <= LOW_SIDE_CAP_DB, "{s:?}");
        }
        assert_eq!(STEREO_BANDS[1..].iter().map(|b| b.1).collect::<Vec<_>>(), STEREO_CROSSOVERS_HZ);
    }
}
