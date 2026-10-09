//! Mix checks: masking between stems (per band and section), mud and harshness per section with the stems that
//! cause them, transient smearing from reverb, and mono compatibility.
//!
//! Masking works on short-time spectra of the stems (8192-point frames, 50 % overlap) in 1/3-octave bands. In a
//! frame, a stem is *prominent* in a band when it carries at least a tenth (-10 dB) of the band's energy summed
//! over all stems, the band is within 30 dB of the frame's strongest band (K-weighted) and the frame is within 50
//! dB of the section's loudest. Two stems *collide* in a band when both are prominent at once; a collision is
//! worse the more often it happens, the closer their levels, the more the band matters to the section's
//! loudness and the less the two are panned apart. A stem is *buried* when little of its (K-weighted) energy is
//! ever prominent.
//!
//! Mud (200-500 Hz) and harshness (2-5 kHz) are loudness-normalised levels (see [`crate::profile`]), judged
//! against pink noise and against the song's other sections. Transient smearing compares onset contrast and
//! the ring after each hit between a dry render and the wet mix at the same onsets; without a dry render the
//! wet values are reported alone.

use crate::audio::AudioRef;
use crate::meta::{Section, check_sections};
use crate::num::{hann_periodic, median, plan};
use crate::profile::{HARSH_HZ, LOW_END_HZ, LOW_END_MATTERS_DB, MUD_HZ, SPECTRUM_N, onsets, pink_level_db, transient_clarity};
use crate::reference::StemShare;
use crate::spectral::{StereoSpectrum, k_weight_power, third_octave_centres, third_octave_edges, third_octave_label};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use rustfft::num_complex::Complex;
use serde::Serialize;

/// Frame length of the masking analysis.
pub const MASK_N: usize = 8192;

/// Level envelopes correlating at least this much make a collision a double.
pub const DOUBLE_CORRELATION: f64 = 0.9;

/// Options of [`check_mix`].
#[derive(Clone, Debug)]
pub struct MixOptions {
    /// Sections to judge separately (empty: the whole mix as one section).
    pub sections: Vec<Section>,
    /// Most collisions reported.
    pub max_conflicts: usize,
    /// Harshness (2-5 kHz) above pink noise by more than this is flagged, dB. Finished mixes measured in
    /// calibration sit between -6 and +2.5; the glockenspiel-led sections the owner calls harsh measure +3.6 to
    /// +4.3.
    pub harsh_vs_pink_db: f64,
    /// Mud (200-500 Hz) above pink noise by more than this is flagged, dB. Orchestral mixes measured in
    /// calibration sit between -5 and +4.8.
    pub mud_vs_pink_db: f64,
    /// The low end (below 120 Hz) losing more than this in a mono fold-down is flagged, dB. 1 dB is a side
    /// signal about 6 dB under the mid.
    pub low_end_mono_loss_db: f64,
    /// A section this far above the song's median mud or harshness is flagged, dB.
    pub section_jump_db: f64,
}

impl Default for MixOptions {
    fn default() -> Self {
        MixOptions {
            sections: vec![],
            max_conflicts: 12,
            harsh_vs_pink_db: 3.0,
            mud_vs_pink_db: 4.0,
            low_end_mono_loss_db: 1.0,
            section_jump_db: 3.0,
        }
    }
}

/// Two stems fighting over a frequency range in a section.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Conflict {
    pub score: f64,
    pub section: String,
    pub stems: [String; 2],
    /// Band of the strongest collision ("250" = the 250 Hz 1/3-octave band).
    pub band: String,
    pub range_hz: [f64; 2],
    /// Share of the section's frames in which both are prominent in the band.
    pub overlap: f64,
    /// Median level of the first stem minus the second in the band while they collide, dB.
    pub level_difference_db: f64,
    /// How far apart they are panned in the band (0 = same position, 2 = opposite sides).
    pub pan_separation: f64,
    /// Correlation of the two stems' level envelopes in the range over the section (dB per frame; `None` when
    /// either is too steady to tell). Near 1, they rise and fall together: a double, or the same rhythm in the
    /// same register.
    pub envelope_correlation: Option<f64>,
    /// `"double"` (they move together: the same line or rhythm twice) or `"collision"` (two parts in each other's
    /// way).
    pub kind: &'static str,
    /// The stem to cut by about 3 dB in the band, when an EQ cut is the advice: the one for which the band
    /// matters less (further under its own strongest band). `None` for doubles, and where the band is both
    /// stems' main energy (a cut would thin either out; the message suggests an octave, panning or a rest).
    pub cut: Option<String>,
    pub message: String,
}

/// A stem that is mostly masked in a section.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Buried {
    pub section: String,
    pub stem: String,
    /// Share of the stem's K-weighted energy that is prominent.
    pub audible_share: f64,
    /// The stem that covers it most.
    pub masked_by: Option<String>,
    pub message: String,
}

/// Mud and harshness of a section.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionTone {
    pub section: String,
    pub start_s: f64,
    pub end_s: f64,
    pub mud_db: Option<f64>,
    pub harsh_db: Option<f64>,
    pub mud_vs_pink_db: Option<f64>,
    pub harsh_vs_pink_db: Option<f64>,
    /// Against the median over the sections, dB.
    pub mud_vs_song_db: Option<f64>,
    pub harsh_vs_song_db: Option<f64>,
    pub mud_stems: Vec<StemShare>,
    pub harsh_stems: Vec<StemShare>,
    pub flags: Vec<String>,
}

/// Mono compatibility of a section.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionMono {
    pub section: String,
    pub fold_down_lu: Option<f64>,
    pub correlation: Option<f64>,
    pub negative_share: f64,
    pub low_end_side_db: Option<f64>,
    /// Power below 120 Hz against the section's, dB.
    pub low_end_db: Option<f64>,
    /// The octave band losing the most in mono, with its loss (dB).
    pub worst_band: Option<(String, f64)>,
    pub flags: Vec<String>,
}

/// Transient clarity of a section, wet against dry when a dry render is given.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionTransients {
    pub section: String,
    pub onsets: usize,
    pub onsets_per_s: f64,
    pub contrast_db: Option<f64>,
    pub tail_db: Option<f64>,
    pub dry_contrast_db: Option<f64>,
    pub dry_tail_db: Option<f64>,
    /// Dry contrast minus wet contrast, dB: how much reverb (and other wet effects) blur the attacks.
    pub smear_db: Option<f64>,
    pub flags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MixReport {
    pub sections: Vec<Section>,
    pub conflicts: Vec<Conflict>,
    pub buried: Vec<Buried>,
    pub tone: Vec<SectionTone>,
    pub mono: Vec<SectionMono>,
    pub transients: Vec<SectionTransients>,
}

/// Per stem: band energy and right-minus-left energy per frame and band.
struct StemFrames {
    energy: Vec<Vec<f64>>,
    pan: Vec<Vec<f64>>,
}

struct Bands {
    edges: Vec<(f64, f64)>,
    labels: Vec<String>,
    /// Bin range of each band.
    bins: Vec<(usize, usize)>,
    /// K-weighting power at each band's centre.
    k: Vec<f64>,
}

fn bands(sr: u32, n: usize) -> Bands {
    let srf = sr as f64;
    let mut b = Bands { edges: vec![], labels: vec![], bins: vec![], k: vec![] };
    for fc in third_octave_centres() {
        let (lo, hi) = third_octave_edges(fc);
        if fc < 40.0 || hi > srf / 2.0 {
            continue;
        }
        let k0 = (lo * n as f64 / srf).ceil() as usize;
        let k1 = ((hi * n as f64 / srf).ceil() as usize).max(k0 + 1);
        b.edges.push((lo, hi));
        b.labels.push(third_octave_label(fc));
        b.bins.push((k0, k1));
        b.k.push(k_weight_power(fc, srf));
    }
    b
}

fn stem_frames(a: AudioRef, bands: &Bands) -> StemFrames {
    let n = MASK_N;
    let hop = n / 2;
    let win = hann_periodic(n);
    let fft = plan(n, false);
    let frames = a.len().div_ceil(hop).max(1);
    let mut energy = vec![vec![0.0; bands.bins.len()]; frames];
    let mut pan = vec![vec![0.0; bands.bins.len()]; frames];
    let mut buf = vec![Complex::new(0.0, 0.0); n];
    for f in 0..frames {
        // frames centred on f * hop
        let start = (f * hop) as isize - (n / 2) as isize;
        for (j, b) in buf.iter_mut().enumerate() {
            let i = start + j as isize;
            *b = if i >= 0 && (i as usize) < a.len() {
                Complex::new(a.left[i as usize] as f64 * win[j], a.right[i as usize] as f64 * win[j])
            } else {
                Complex::new(0.0, 0.0)
            };
        }
        fft.process(&mut buf);
        for (bi, &(k0, k1)) in bands.bins.iter().enumerate() {
            let (mut e, mut p) = (0.0, 0.0);
            for k in k0..k1.min(n / 2 + 1) {
                let x = buf[k];
                let y = buf[(n - k) % n].conj();
                let l = ((x + y) * 0.5).norm_sqr();
                let r = ((x - y) * 0.5).norm_sqr();
                e += (l + r) * 0.5;
                p += r - l;
            }
            energy[f][bi] = e;
            pan[f][bi] = p;
        }
    }
    StemFrames { energy, pan }
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn db10(x: f64) -> Option<f64> {
    (x > 1e-30 && x.is_finite()).then(|| 10.0 * x.log10())
}

fn hz(f: f64) -> String {
    if f >= 1000.0 { format!("{:.1} kHz", f / 1000.0).replace(".0 kHz", " kHz") } else { format!("{f:.0} Hz") }
}

/// Pearson correlation of two stems' level envelopes (dB of the energy summed over bands `b0..=b1`) over the
/// `frames`; `None` when either moves less than 1 dB (standard deviation), where correlation means nothing.
fn envelope_correlation(x: &StemFrames, y: &StemFrames, frames: &[usize], b0: usize, b1: usize) -> Option<f64> {
    let level = |s: &StemFrames| -> Vec<f64> {
        let e: Vec<f64> = frames.iter().map(|&f| s.energy[f][b0..=b1].iter().sum::<f64>()).collect();
        // 60 dB under the stem's loudest frame counts as silence
        let floor = e.iter().copied().fold(0.0, f64::max) * 1e-6 + 1e-30;
        e.iter().map(|v| 10.0 * (v + floor).log10()).collect()
    };
    let (a, b) = (level(x), level(y));
    let n = a.len() as f64;
    if a.len() < 4 {
        return None;
    }
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (p, q) in a.iter().zip(&b) {
        sab += (p - ma) * (q - mb);
        saa += (p - ma) * (p - ma);
        sbb += (q - mb) * (q - mb);
    }
    ((saa / n).sqrt() >= 1.0 && (sbb / n).sqrt() >= 1.0).then(|| sab / (saa * sbb).sqrt())
}

/// Collisions and buried stems in one section (frames `f0..f1`).
#[allow(clippy::too_many_arguments)]
fn masking(
    names: &[String],
    st: &[StemFrames],
    bands: &Bands,
    section: &str,
    f0: usize,
    f1: usize,
    conflicts: &mut Vec<Conflict>,
    buried: &mut Vec<Buried>,
) {
    let nb = bands.bins.len();
    let ns = names.len();
    if f1 <= f0 || ns < 2 {
        return;
    }
    // frame weights: K-weighted total per frame, and its loudest band
    let total = |f: usize, b: usize| -> f64 { st.iter().map(|s| s.energy[f][b]).sum() };
    let kframe: Vec<f64> = (f0..f1).map(|f| (0..nb).map(|b| total(f, b) * bands.k[b]).sum()).collect();
    let loudest = kframe.iter().copied().fold(0.0, f64::max);
    if loudest <= 1e-30 {
        return;
    }
    let mut band_k = vec![0.0; nb];
    // prominent[s][frame][band]
    let mut prom = vec![vec![vec![false; nb]; f1 - f0]; ns];
    for (fi, f) in (f0..f1).enumerate() {
        if kframe[fi] < loudest * 1e-5 {
            continue;
        }
        let tb: Vec<f64> = (0..nb).map(|b| total(f, b)).collect();
        let kmax = (0..nb).map(|b| tb[b] * bands.k[b]).fold(0.0, f64::max);
        for b in 0..nb {
            band_k[b] += tb[b] * bands.k[b];
            if tb[b] * bands.k[b] < kmax * 1e-3 {
                continue;
            }
            for s in 0..ns {
                prom[s][fi][b] = st[s].energy[f][b] >= 0.1 * tb[b];
            }
        }
    }
    let ksum: f64 = band_k.iter().sum();
    let frames = (f1 - f0) as f64;
    let active: Vec<usize> = (f0..f1).filter(|&f| kframe[f - f0] >= loudest * 1e-5).collect();
    // each stem's level per band in the section, for choosing which to cut
    let stem_band: Vec<Vec<f64>> = (0..ns).map(|s| (0..nb).map(|b| (f0..f1).map(|f| st[s].energy[f][b]).sum::<f64>()).collect()).collect();
    let rel_to_own_peak = |s: usize, b: usize| -> f64 {
        let peak = stem_band[s].iter().zip(&bands.k).map(|(e, k)| e * k).fold(0.0, f64::max);
        db10(stem_band[s][b] * bands.k[b] / peak.max(1e-30)).unwrap_or(-120.0)
    };
    for a in 0..ns {
        for c in a + 1..ns {
            // score per band, then report runs of neighbouring bands as one collision
            let mut per_band: Vec<Option<(f64, f64, f64, f64)>> = vec![None; nb];
            for b in 0..nb {
                let mut diffs = vec![];
                let (mut pa, mut pc, mut ea, mut ec) = (0.0, 0.0, 0.0, 0.0);
                for (fi, f) in (f0..f1).enumerate() {
                    if prom[a][fi][b] && prom[c][fi][b] {
                        let (x, y) = (st[a].energy[f][b], st[c].energy[f][b]);
                        diffs.push(10.0 * (x.max(1e-30) / y.max(1e-30)).log10());
                        pa += st[a].pan[f][b];
                        pc += st[c].pan[f][b];
                        ea += x;
                        ec += y;
                    }
                }
                if diffs.is_empty() {
                    continue;
                }
                let overlap = diffs.len() as f64 / frames;
                let d = median(&diffs);
                // pan position (R - L) / (R + L) of each in the band; band energy is (L + R) / 2
                let sep = (pa / (2.0 * ea).max(1e-30) - pc / (2.0 * ec).max(1e-30)).abs();
                let share = band_k[b] / ksum.max(1e-30);
                let w_band = (share * nb as f64).clamp(0.1, 4.0).sqrt();
                let score = 100.0 * overlap * (1.0 - d.abs() / 10.0).max(0.0) * (1.0 - 0.5 * sep) * w_band;
                per_band[b] = Some((score, overlap, d, sep));
            }
            let mut b = 0;
            while b < nb {
                if per_band[b].is_none_or(|x| x.0 < 10.0) {
                    b += 1;
                    continue;
                }
                let mut e = b;
                while e + 1 < nb && per_band[e + 1].is_some_and(|x| x.0 >= 10.0) {
                    e += 1;
                }
                let best = (b..=e).max_by(|&i, &j| per_band[i].unwrap().0.total_cmp(&per_band[j].unwrap().0)).unwrap_or(b);
                let (score, overlap, d, sep) = per_band[best].unwrap();
                let (lo, hi) = (bands.edges[b].0, bands.edges[e].1);
                let (ra, rc) = (rel_to_own_peak(a, best), rel_to_own_peak(c, best));
                let cut = if (ra - rc).abs() > 1.0 { if ra < rc { a } else { c } } else if d < 0.0 { a } else { c };
                let keep = if cut == a { c } else { a };
                let louder = if d >= 0.0 { a } else { c };
                let fc = (bands.edges[best].0 * bands.edges[best].1).sqrt();
                let corr = envelope_correlation(&st[a], &st[c], &active, b, e);
                let double = corr.is_some_and(|r| r >= DOUBLE_CORRELATION);
                let what = format!(
                    "{}-{} in {} ({:.0} % of the time, {}{})",
                    hz(lo),
                    hz(hi),
                    section,
                    overlap * 100.0,
                    if d.abs() < 0.05 { "equally loud".to_string() } else { format!("{} {:.1} dB louder", names[louder], d.abs()) },
                    if sep > 0.3 { format!(", panned {sep:.1} apart") } else { String::new() },
                );
                let below_own = (-rel_to_own_peak(cut, best)).max(0.0);
                let eq_cut = !double && below_own >= 3.0;
                let message = if double {
                    format!(
                        "{} and {} move together over {what}: the same line or rhythm in the same register (level envelopes correlate {:.2}). If the double is meant, pan them apart{} or vary one (octave, articulation, timing); if not, give one its own register or rhythm, or drop it",
                        names[a],
                        names[c],
                        corr.unwrap_or(1.0),
                        if sep <= 0.3 { " (they sit together now)" } else { "" },
                    )
                } else if below_own < 3.0 {
                    format!(
                        "{} and {} fight over {what}: both have their main energy here, so an EQ cut would thin {} out; move one part by an octave, pan them apart, or let one rest here",
                        names[a], names[c], names[cut]
                    )
                } else {
                    format!(
                        "{} and {} fight over {what}: cut {} by about 3 dB around {} (a minor band for it, {:.0} dB under its strongest, so it loses little; {} keeps it), or move one part by an octave or pan them apart",
                        names[a], names[c], names[cut], hz(fc), below_own, names[keep]
                    )
                };
                conflicts.push(Conflict {
                    score: r1(score),
                    section: section.to_string(),
                    stems: [names[a].clone(), names[c].clone()],
                    band: bands.labels[best].clone(),
                    range_hz: [lo.round(), hi.round()],
                    overlap: r2(overlap),
                    level_difference_db: r1(d),
                    pan_separation: r2(sep),
                    envelope_correlation: corr.map(r2),
                    kind: if double { "double" } else { "collision" },
                    cut: eq_cut.then(|| names[cut].clone()),
                    message,
                });
                b = e + 1;
            }
        }
    }
    let kstem = |s: usize, f: usize| -> f64 { (0..nb).map(|b| st[s].energy[f][b] * bands.k[b]).sum() };
    for s in 0..ns {
        // a stem that only rings on here (the tail of notes before the section) or plays a hit or two is not buried:
        // it has to sound, within 30 dB of its loudest anywhere in the song, for a fifth of the section
        let peak = (0..st[s].energy.len()).map(|f| kstem(s, f)).fold(0.0, f64::max);
        let sounding = (f0..f1).filter(|&f| kstem(s, f) >= peak * 1e-3).count();
        if (sounding as f64) < 0.2 * frames {
            continue;
        }
        let (mut all, mut heard) = (0.0, 0.0);
        let mut cover = vec![0.0; ns];
        for (fi, f) in (f0..f1).enumerate() {
            for b in 0..nb {
                let e = st[s].energy[f][b] * bands.k[b];
                all += e;
                if prom[s][fi][b] {
                    heard += e;
                } else {
                    for (o, c) in cover.iter_mut().enumerate() {
                        if o != s {
                            *c += st[o].energy[f][b] * bands.k[b];
                        }
                    }
                }
            }
        }
        // a stem that barely plays here is not buried, just quiet
        let section_k: f64 = band_k.iter().sum();
        if all < section_k * 1e-4 || all <= 1e-30 {
            continue;
        }
        let share = heard / all;
        if share < 0.25 {
            let by = (0..ns).filter(|&o| o != s).max_by(|&x, &y| cover[x].total_cmp(&cover[y])).map(|o| names[o].clone());
            buried.push(Buried {
                section: section.to_string(),
                stem: names[s].clone(),
                audible_share: r2(share),
                masked_by: by.clone(),
                message: format!(
                    "{} is mostly masked in {}: only {:.0} % of its energy stands within 10 dB of the other parts in its bands{}; raise it, carve space for it, or drop it if it adds nothing",
                    names[s],
                    section,
                    share * 100.0,
                    by.map(|b| format!(" ({b} covers it most)")).unwrap_or_default()
                ),
            });
        }
    }
}

fn shares(stems: &[(String, StereoSpectrum)], lo: f64, hi: f64) -> Vec<StemShare> {
    let p: Vec<(String, f64)> = stems.iter().map(|(n, s)| (n.clone(), s.band(lo, hi).power())).collect();
    let tot: f64 = p.iter().map(|x| x.1).sum();
    if tot <= 1e-30 {
        return vec![];
    }
    let mut v: Vec<StemShare> = p.into_iter().map(|(stem, e)| StemShare { stem, share: r2(e / tot) }).filter(|s| s.share >= 0.1).collect();
    v.sort_by(|a, b| b.share.total_cmp(&a.share));
    v.truncate(3);
    v
}

fn names_of(v: &[StemShare]) -> String {
    if v.is_empty() {
        return String::new();
    }
    format!(" ({})", v.iter().map(|s| format!("{} {:.0} %", s.stem, s.share * 100.0)).collect::<Vec<_>>().join(", "))
}

/// Run every check. `mix` is the master (or the sum of the stems); `stems` are the named parts (same rate and
/// length as the mix) for masking and attribution; `dry` is the mix rendered without reverb (and other wet
/// effects), for transient smearing.
pub fn check_mix(mix: AudioRef, stems: &[(&str, AudioRef)], dry: Option<AudioRef>, o: &MixOptions) -> Result<MixReport> {
    ensure!(mix.len() >= mix.sr as usize / 2, "the mix is too short to check (at least 0.5 s)");
    for (n, s) in stems {
        ensure!(s.sr == mix.sr, "stem '{n}' is {} Hz, the mix {} Hz", s.sr, mix.sr);
    }
    if let Some(d) = dry {
        ensure!(d.sr == mix.sr && d.len() == mix.len(), "the dry render must match the mix's rate and length");
    }
    let sections = if o.sections.is_empty() { vec![Section::whole(mix)] } else { o.sections.clone() };
    check_sections(&sections, mix.duration_s())?;
    let names: Vec<String> = stems.iter().map(|(n, _)| n.to_string()).collect();

    // masking
    let bands = bands(mix.sr, MASK_N);
    let st: Vec<StemFrames> = stems.par_iter().map(|(_, a)| stem_frames(*a, &bands)).collect();
    let hop = (MASK_N / 2) as f64 / mix.sr as f64;
    let (mut conflicts, mut buried) = (vec![], vec![]);
    if stems.len() >= 2 {
        let nframes = st.iter().map(|s| s.energy.len()).min().unwrap_or(0);
        for s in &sections {
            let f0 = ((s.start_s / hop).round() as usize).min(nframes);
            let f1 = ((s.end_s / hop).round() as usize).min(nframes);
            masking(&names, &st, &bands, &s.name, f0, f1, &mut conflicts, &mut buried);
        }
    }
    conflicts.sort_by(|a, b| b.score.total_cmp(&a.score));
    conflicts.truncate(o.max_conflicts);

    // tone per section
    let sr = mix.sr as f64;
    let all_onsets = onsets(dry.unwrap_or(mix));
    let one = |s: &Section| -> Result<Option<(SectionTone, SectionMono, SectionTransients)>> {
        let a = mix.section(s);
        if a.len() < mix.sr as usize / 2 {
            return Ok(None);
        }
        let spec = StereoSpectrum::new(a, SPECTRUM_N);
        let k = spec.k_weighted_power();
        let lv = |(lo, hi): (f64, f64)| db10(spec.band(lo, hi).power() / k);
        let stem_specs: Vec<(String, StereoSpectrum)> =
            stems.par_iter().map(|(n, x)| (n.to_string(), StereoSpectrum::new(x.section(s), SPECTRUM_N))).collect();
        let (mud, harsh) = (lv(MUD_HZ), lv(HARSH_HZ));
        let tone = SectionTone {
            section: s.name.clone(),
            start_s: s.start_s,
            end_s: s.end_s,
            mud_db: mud.map(r1),
            harsh_db: harsh.map(r1),
            mud_vs_pink_db: mud.map(|m| r1(m - pink_level_db(MUD_HZ.0, MUD_HZ.1, sr))),
            harsh_vs_pink_db: harsh.map(|h| r1(h - pink_level_db(HARSH_HZ.0, HARSH_HZ.1, sr))),
            mud_vs_song_db: None,
            harsh_vs_song_db: None,
            mud_stems: shares(&stem_specs, MUD_HZ.0, MUD_HZ.1),
            harsh_stems: shares(&stem_specs, HARSH_HZ.0, HARSH_HZ.1),
            flags: vec![],
        };

        // mono compatibility
        let p = crate::profile::profile(a).with_context(|| format!("section '{}'", s.name))?;
        let loudest = p.octaves.iter().filter_map(|b| b.level_db).fold(f64::NEG_INFINITY, f64::max);
        let worst = p
            .octaves
            .iter()
            .filter(|b| b.level_db.is_some_and(|l| l > loudest - 20.0))
            .filter_map(|b| b.mono_loss_db.map(|m| (b.band.clone(), r1(m))))
            .min_by(|x, y| x.1.total_cmp(&y.1));
        let mut mflags = vec![];
        if let Some(f) = p.mono.fold_down_lu
            && f < -4.0
        {
            mflags.push(format!("the mono fold-down is {:.1} LU quieter: parts cancel between the channels (phase-inverted or Haas-widened)", -f));
        }
        if let Some((b, l)) = &worst
            && *l < -6.0
        {
            mflags.push(format!("the {b} Hz octave loses {:.1} dB in mono: something there is out of phase between left and right", -l));
        }
        // side power lost in a mono fold-down, relative to mid + side (where there is a low end to lose)
        if let Some(ls) = p.mono.low_end_side_db
            && p.mono.low_end_db.is_some_and(|l| l > LOW_END_MATTERS_DB)
        {
            let loss = 10.0 * (1.0 + 10f64.powf(ls / 10.0)).log10();
            if loss > o.low_end_mono_loss_db {
                mflags.push(format!(
                    "the low end (below {LOW_END_HZ:.0} Hz) loses {loss:.1} dB in mono (side only {:.1} dB under mid): narrow the bass parts or mono them below {LOW_END_HZ:.0} Hz (set_mixer mono_below_hz)",
                    -ls
                ));
            }
        }
        if p.mono.negative_share > 0.1 {
            mflags.push(format!("{:.0} % of the time the channels are negatively correlated", p.mono.negative_share * 100.0));
        }
        let mono = SectionMono {
            section: s.name.clone(),
            fold_down_lu: p.mono.fold_down_lu.map(r1),
            correlation: p.mono.correlation.map(r2),
            negative_share: r2(p.mono.negative_share),
            low_end_side_db: p.mono.low_end_side_db.map(r1),
            low_end_db: p.mono.low_end_db.map(r1),
            worst_band: worst,
            flags: mflags,
        };

        // transients at the onsets inside the section
        let (i0, i1) = s.samples(mix.sr, mix.len());
        let on: Vec<usize> = all_onsets.iter().copied().filter(|&x| x >= i0 && x < i1).collect();
        let (wc, wt) = transient_clarity(mix, &on);
        let (dc, dt) = dry.map(|d| transient_clarity(d, &on)).unwrap_or((None, None));
        let smear = match (dc, wc) {
            (Some(d), Some(w)) => Some(r1(d - w)),
            _ => None,
        };
        let mut tflags = vec![];
        if let Some(sm) = smear
            && sm > 6.0
        {
            tflags.push(format!("reverb blurs the attacks: transients stand out {sm:.1} dB less than dry; shorten the decay, add pre-delay, or send less of the percussive parts"));
        }
        if let (Some(d), Some(w)) = (dt, wt)
            && w - d > 6.0
        {
            tflags.push(format!("hits ring {:.1} dB louder 100-150 ms after the attack than dry", w - d));
        }
        let transients = SectionTransients {
            section: s.name.clone(),
            onsets: on.len(),
            onsets_per_s: r2(on.len() as f64 / s.duration_s()),
            contrast_db: wc.map(r1),
            tail_db: wt.map(r1),
            dry_contrast_db: dc.map(r1),
            dry_tail_db: dt.map(r1),
            smear_db: smear,
            flags: tflags,
        };
        Ok(Some((tone, mono, transients)))
    };
    let per_section: Vec<_> = sections.par_iter().map(one).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect();
    let mut tone = Vec::with_capacity(per_section.len());
    let mut mono = Vec::with_capacity(per_section.len());
    let mut transients = Vec::with_capacity(per_section.len());
    for (t, m, x) in per_section {
        tone.push(t);
        mono.push(m);
        transients.push(x);
    }
    // tone against the song and the thresholds
    let med = |v: Vec<f64>| (!v.is_empty()).then(|| median(&v));
    let mud_med = med(tone.iter().filter_map(|t| t.mud_db).collect());
    let harsh_med = med(tone.iter().filter_map(|t| t.harsh_db).collect());
    let several = tone.len() > 1;
    for t in tone.iter_mut() {
        if several {
            t.mud_vs_song_db = t.mud_db.zip(mud_med).map(|(a, b)| r1(a - b));
            t.harsh_vs_song_db = t.harsh_db.zip(harsh_med).map(|(a, b)| r1(a - b));
        }
        if let Some(h) = t.harsh_vs_pink_db
            && h > o.harsh_vs_pink_db
        {
            t.flags.push(format!("harsh: 2-5 kHz is {h:+.1} dB against pink noise{}", names_of(&t.harsh_stems)));
        }
        if let Some(m) = t.mud_vs_pink_db
            && m > o.mud_vs_pink_db
        {
            t.flags.push(format!("muddy: 200-500 Hz is {m:+.1} dB against pink noise{}", names_of(&t.mud_stems)));
        }
        if let Some(h) = t.harsh_vs_song_db
            && h > o.section_jump_db
        {
            t.flags.push(format!("{h:.1} dB more 2-5 kHz than the song's other sections{}", names_of(&t.harsh_stems)));
        }
        if let Some(m) = t.mud_vs_song_db
            && m > o.section_jump_db
        {
            t.flags.push(format!("{m:.1} dB more 200-500 Hz than the song's other sections{}", names_of(&t.mud_stems)));
        }
    }
    Ok(MixReport { sections, conflicts, buried, tone, mono, transients })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::tests::{noise, pink};

    /// White noise limited to `lo..hi` Hz by zeroing the other bins of its spectrum.
    fn band_noise(n: usize, seed: u32, sr: f64, lo: f64, hi: f64) -> Vec<f32> {
        let x: Vec<f64> = noise(n, seed).iter().map(|&v| v as f64).collect();
        let mut sp = crate::num::rfft(&x, n);
        for (k, c) in sp.iter_mut().enumerate() {
            let f = k as f64 * sr / n as f64;
            if f < lo || f >= hi {
                *c = Complex::new(0.0, 0.0);
            }
        }
        crate::num::irfft(&sp, n).iter().map(|&v| v as f32).collect()
    }

    #[test]
    fn finds_two_parts_in_the_same_band_and_a_buried_one() {
        let sr = 48000u32;
        let n = sr as usize * 8;
        // two parts at the same level around 300 Hz, one part alone around 3 kHz, and a quiet one under the first two
        let srf = sr as f64;
        let a = band_noise(n, 1, srf, 220.0, 400.0);
        let b = band_noise(n, 2, srf, 220.0, 400.0);
        // ten times the bandwidth: scaled to the same power as the others
        let c: Vec<f32> = band_noise(n, 3, srf, 2200.0, 4000.0).iter().map(|v| v * 0.1f32.sqrt()).collect();
        let d: Vec<f32> = band_noise(n, 4, srf, 220.0, 400.0).iter().map(|v| v * 0.1).collect();
        let mix: Vec<f32> = (0..n).map(|i| a[i] + b[i] + c[i] + d[i]).collect();
        let r = |x: &[f32]| -> Vec<f32> { x.to_vec() };
        let (a2, b2, c2, d2) = (r(&a), r(&b), r(&c), r(&d));
        let st = [
            ("cello", AudioRef::new(sr, &a, &a2).unwrap()),
            ("horn", AudioRef::new(sr, &b, &b2).unwrap()),
            ("flute", AudioRef::new(sr, &c, &c2).unwrap()),
            ("pad", AudioRef::new(sr, &d, &d2).unwrap()),
        ];
        let o = MixOptions { sections: vec![Section::new("A", 0.0, 4.0), Section::new("B", 4.0, 8.0)], ..Default::default() };
        let rep = check_mix(AudioRef::new(sr, &mix, &mix).unwrap(), &st, None, &o).unwrap();
        let top = &rep.conflicts[0];
        assert_eq!(top.stems, ["cello".to_string(), "horn".to_string()], "{:#?}", rep.conflicts);
        assert!(top.range_hz[0] < 300.0 && top.range_hz[1] > 300.0, "{top:?}");
        assert!(top.overlap > 0.8 && top.level_difference_db.abs() < 2.0, "{top:?}");
        assert!(rep.conflicts.iter().all(|c| !c.stems.contains(&"flute".to_string())), "{:#?}", rep.conflicts);
        assert!(rep.buried.iter().any(|b| b.stem == "pad"), "{:#?}", rep.buried);
        assert!(!rep.buried.iter().any(|b| b.stem == "flute"));
        assert_eq!(rep.tone.len(), 2);
        assert_eq!(rep.tone[0].mud_stems.len(), 2, "{:?}", rep.tone[0]);
        assert!(rep.tone[0].flags.iter().any(|f| f.starts_with("muddy")), "{:?}", rep.tone[0]);
    }

    #[test]
    fn the_tail_of_a_part_that_stopped_is_not_buried() {
        let sr = 48000u32;
        let srf = sr as f64;
        let n = sr as usize * 8;
        // a cello all through; a piano that plays most of the first section at the cello's level, then only its
        // release rings on, 15 dB under the cello and dying away 20 dB a second
        let cello = band_noise(n, 1, srf, 220.0, 400.0);
        let piano: Vec<f32> = band_noise(n, 2, srf, 220.0, 400.0)
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let t = i as f64 / srf;
                if t < 3.6 { *v } else { v * 10f64.powf((-15.0 - 20.0 * (t - 3.6)) / 20.0) as f32 }
            })
            .collect();
        let mix: Vec<f32> = cello.iter().zip(&piano).map(|(a, b)| a + b).collect();
        let st = [("cello", AudioRef::new(sr, &cello, &cello).unwrap()), ("piano", AudioRef::new(sr, &piano, &piano).unwrap())];
        let o = MixOptions { sections: vec![Section::new("A", 0.0, 4.0), Section::new("B", 4.0, 8.0)], ..Default::default() };
        let rep = check_mix(AudioRef::new(sr, &mix, &mix).unwrap(), &st, None, &o).unwrap();
        assert!(rep.buried.is_empty(), "{:#?}", rep.buried);
    }

    #[test]
    fn level_envelopes_correlate() {
        let frames = |e: &[f64]| StemFrames { energy: e.iter().map(|&v| vec![v, v]).collect(), pan: vec![vec![0.0; 2]; e.len()] };
        let up_down = frames(&[1.0, 100.0, 1.0, 100.0, 1.0, 100.0]);
        let down_up = frames(&[100.0, 1.0, 100.0, 1.0, 100.0, 1.0]);
        let steady = frames(&[50.0, 51.0, 50.0, 49.0, 50.0, 50.0]);
        let all: Vec<usize> = (0..6).collect();
        let r = envelope_correlation(&up_down, &up_down, &all, 0, 1).unwrap();
        assert!((r - 1.0).abs() < 1e-12, "{r}");
        let r = envelope_correlation(&up_down, &down_up, &all, 0, 0).unwrap();
        assert!((r + 1.0).abs() < 1e-12, "{r}");
        // a steady part has no envelope to correlate, and three frames are too few to tell
        assert_eq!(envelope_correlation(&up_down, &steady, &all, 0, 1), None);
        assert_eq!(envelope_correlation(&up_down, &up_down, &[0, 1, 2], 0, 1), None);
        // silence is floored 60 dB under the stem's loudest frame, not minus infinity
        let gaps = frames(&[0.0, 100.0, 0.0, 100.0, 0.0, 100.0]);
        let r = envelope_correlation(&up_down, &gaps, &all, 0, 1).unwrap();
        assert!(r.is_finite() && r > 0.99, "{r}");
    }

    #[test]
    fn a_double_is_told_from_a_collision() {
        let sr = 48000u32;
        let n = sr as usize * 8;
        let srf = sr as f64;
        // two parts playing the same rhythm (0.5 s on, 0.5 s 20 dB down) in the same register, and a steady third
        let gate = |x: Vec<f32>| -> Vec<f32> { x.iter().enumerate().map(|(i, v)| if (i / (sr as usize / 2)).is_multiple_of(2) { *v } else { v * 0.1 }).collect() };
        let a = gate(band_noise(n, 11, srf, 220.0, 400.0));
        let b = gate(band_noise(n, 12, srf, 220.0, 400.0));
        let c = band_noise(n, 13, srf, 220.0, 400.0);
        let mix: Vec<f32> = (0..n).map(|i| a[i] + b[i] + c[i]).collect();
        let st = [
            ("violins", AudioRef::new(sr, &a, &a).unwrap()),
            ("flutes", AudioRef::new(sr, &b, &b).unwrap()),
            ("organ", AudioRef::new(sr, &c, &c).unwrap()),
        ];
        let rep = check_mix(AudioRef::new(sr, &mix, &mix).unwrap(), &st, None, &MixOptions::default()).unwrap();
        let find = |x: &str, y: &str| rep.conflicts.iter().find(|c| c.stems == [x.to_string(), y.to_string()]);
        let double = find("violins", "flutes").unwrap_or_else(|| panic!("{:#?}", rep.conflicts));
        assert_eq!(double.kind, "double", "{double:?}");
        assert!(double.envelope_correlation.unwrap() >= DOUBLE_CORRELATION, "{double:?}");
        assert!(double.message.contains("move together") && double.message.contains("they sit together now"), "{}", double.message);
        let clash = find("violins", "organ").unwrap_or_else(|| panic!("{:#?}", rep.conflicts));
        assert_eq!(clash.kind, "collision", "{clash:?}");
        assert!(clash.envelope_correlation.is_none_or(|r| r < DOUBLE_CORRELATION), "{clash:?}");
        // the band is the organ's and the violins' main energy: no EQ cut is suggested
        assert!(clash.message.contains("would thin"), "{}", clash.message);
        assert_eq!((&double.cut, &clash.cut), (&None, &None));
    }

    #[test]
    fn an_eq_cut_goes_to_the_part_the_band_matters_less_to() {
        let sr = 48000u32;
        let n = sr as usize * 4;
        let srf = sr as f64;
        // the organ lives around 300 Hz; the bell lives around 3 kHz and has a minor part around 300 Hz
        let organ = band_noise(n, 31, srf, 220.0, 400.0);
        let low = band_noise(n, 32, srf, 220.0, 400.0);
        let high = band_noise(n, 33, srf, 2200.0, 4000.0);
        let bell: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + 3.0 * b).collect();
        let mix: Vec<f32> = organ.iter().zip(&bell).map(|(a, b)| a + b).collect();
        let st = [("organ", AudioRef::new(sr, &organ, &organ).unwrap()), ("bell", AudioRef::new(sr, &bell, &bell).unwrap())];
        let rep = check_mix(AudioRef::new(sr, &mix, &mix).unwrap(), &st, None, &MixOptions::default()).unwrap();
        let c = &rep.conflicts[0];
        assert_eq!((c.kind, c.cut.as_deref()), ("collision", Some("bell")), "{c:#?}");
        assert!(c.range_hz[0] < 300.0 && c.range_hz[1] > 300.0, "{c:?}");
        assert!(c.message.contains("cut bell by about 3 dB") && c.message.contains("organ keeps it"), "{}", c.message);
    }

    #[test]
    fn reverb_smear_and_mono_problems_are_reported() {
        let sr = 48000u32;
        let n = sr as usize * 6;
        let nz = noise(n, 9);
        let hits = |decay: f32| -> Vec<f32> {
            let mut x = vec![0f32; n];
            for h in 0..12 {
                let s = h * sr as usize / 2 + 2400;
                for j in 0..(sr as usize / 2).min(n - s) {
                    x[s + j] += nz[s + j] * (-(j as f32) / (decay * sr as f32)).exp();
                }
            }
            x
        };
        let dry = hits(0.015);
        let wet = hits(0.3);
        let inv: Vec<f32> = wet.iter().map(|v| -v).collect();
        let rep = check_mix(AudioRef::new(sr, &wet, &inv).unwrap(), &[], Some(AudioRef::new(sr, &dry, &dry).unwrap()), &MixOptions::default()).unwrap();
        let t = &rep.transients[0];
        assert!(t.smear_db.unwrap() > 6.0 && !t.flags.is_empty(), "{t:?}");
        let m = &rep.mono[0];
        assert_eq!(m.fold_down_lu, Some(-120.0));
        assert!(m.flags.len() >= 2, "{m:?}");
        assert!(rep.conflicts.is_empty() && rep.buried.is_empty());
        let p = pink(n, 3);
        let rep = check_mix(AudioRef::new(sr, &p, &p).unwrap(), &[], None, &MixOptions::default()).unwrap();
        assert!(rep.mono[0].flags.is_empty(), "{:?}", rep.mono[0]);
        assert!(rep.tone[0].flags.iter().all(|f| !f.starts_with("muddy")), "{:?}", rep.tone[0]);
    }
}
