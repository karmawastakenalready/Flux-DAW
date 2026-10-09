//! Per-section level of each stem: the mix balance table (the port of `tools/stem_balance.py`).
//!
//! The Python tool also ran each stem through the pipeline's mix chain (HPF, EQ, gain, creative effects, time
//! stops, ducking) before measuring. In Flux the mixer renders processed audio already, so this takes stems as
//! they sound in the mix: a render's layer stems, exported layer files, or any post-fader stems.

use crate::audio::AudioRef;
use crate::meta::{Section, check_sections};
use crate::num::np_sum;
use anyhow::{Result, ensure};
use serde::Serialize;

/// One stem's row of the table.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StemRow {
    pub name: String,
    /// Mono RMS per section in dBFS (`20 log10(rms + 1e-9)`), in section order.
    pub rms_db: Vec<f64>,
}

/// The balance table: stems sorted loudest first (by their loudest section).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StemBalance {
    pub sections: Vec<String>,
    pub stems: Vec<StemRow>,
    /// Stems left out because they are silent (peak below 1e-7).
    pub silent: Vec<String>,
}

impl StemBalance {
    /// The table as text, laid out like the Python tool's output.
    pub fn table(&self) -> String {
        let mut s = format!("{:12}", "stem");
        for n in &self.sections {
            let short: String = n.chars().take(9).collect();
            s += &format!("{short:>10}");
        }
        s.push('\n');
        for r in &self.stems {
            s += &format!("{:12}", r.name);
            for v in &r.rms_db {
                s += &format!("{v:10.1}");
            }
            s.push('\n');
        }
        s
    }
}

/// Mono signal of a stem, with everything past `loop_len` samples folded back onto the start (a loop's tail
/// rings into its next pass, so it belongs to the opening bars).
pub fn folded_mid(a: AudioRef, loop_len: Option<usize>) -> Vec<f64> {
    let mut y: Vec<f64> = (0..a.len()).map(|i| a.mid(i)).collect();
    if let Some(l) = loop_len
        && y.len() > l
    {
        let tail = y.split_off(l);
        for (i, v) in tail.iter().take(l).enumerate() {
            y[i] += v;
        }
    }
    y
}

/// Section RMS of each stem in dBFS. `loop_len` (samples) folds the tail of looped stems back onto the start.
pub fn stem_balance(stems: &[(&str, AudioRef)], sections: &[Section], loop_len: Option<usize>) -> Result<StemBalance> {
    ensure!(!sections.is_empty(), "no sections to measure");
    let mut rows = vec![];
    let mut silent = vec![];
    for (name, a) in stems {
        ensure!(stems.iter().all(|(_, b)| b.sr == a.sr), "stems have different sample rates");
        let dur = loop_len.map(|l| l as f64 / a.sr as f64).unwrap_or(a.duration_s());
        check_sections(sections, dur.max(a.duration_s()))?;
        if a.sample_peak() < 1e-7 {
            silent.push(name.to_string());
            continue;
        }
        let y = folded_mid(*a, loop_len);
        let rms_db = sections
            .iter()
            .map(|s| {
                let (i, j) = s.samples(a.sr, y.len());
                let sq: Vec<f64> = y[i..j].iter().map(|v| v * v).collect();
                let rms = if sq.is_empty() { 0.0 } else { (np_sum(&sq) / sq.len() as f64).sqrt() };
                20.0 * (rms + 1e-9).log10()
            })
            .collect();
        rows.push(StemRow { name: name.to_string(), rms_db });
    }
    let loudest = |r: &StemRow| r.rms_db.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    rows.sort_by(|a, b| loudest(b).total_cmp(&loudest(a)));
    Ok(StemBalance { sections: sections.iter().map(|s| s.name.clone()).collect(), stems: rows, silent })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_the_tail_and_ranks_stems() {
        let sr = 100u32;
        // 2 s loop with a 0.5 s tail; the tail adds onto the first 0.5 s
        let a = vec![0.1f32; 250];
        let b = vec![0.01f32; 200];
        let z = vec![0f32; 200];
        let stems = [
            ("quiet", AudioRef::new(sr, &b, &b).unwrap()),
            ("loud", AudioRef::new(sr, &a, &a).unwrap()),
            ("silent", AudioRef::new(sr, &z, &z).unwrap()),
        ];
        let secs = [Section::new("A", 0.0, 0.5), Section::new("B", 0.5, 2.0)];
        let t = stem_balance(&stems, &secs, Some(200)).unwrap();
        assert_eq!(t.silent, vec!["silent"]);
        assert_eq!(t.stems[0].name, "loud");
        assert!((t.stems[0].rms_db[0] - 20.0 * 0.2f64.log10()).abs() < 1e-6, "{:?}", t.stems[0]);
        assert!((t.stems[0].rms_db[1] - 20.0 * 0.1f64.log10()).abs() < 1e-6);
        assert!((t.stems[1].rms_db[1] + 40.0).abs() < 1e-6);
        assert!(t.table().starts_with("stem                 A         B\nloud"));
        assert!(stem_balance(&stems, &[], None).is_err());
        assert!(stem_balance(&stems, &[Section::new("late", 5.0, 6.0)], None).is_err());
    }
}
