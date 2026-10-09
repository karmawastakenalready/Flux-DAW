//! Voice leading and bass lines: score.py's `voice_lead` and `bass_line`, ported operation for operation so
//! the same chords give the same pitches.
//!
//! **Voice leading** chooses one voicing per chord with the least total movement. Every candidate voicing
//! stays within `[low, high]`, holds the chord's essential tones (a thin voicing drops the fifth first,
//! then extensions, then the root), never doubles the third of a major chord (and leaves it out over a
//! first-inversion bass that already has it), and pays for doubled sevenths and tensions, gaps wider than
//! an octave between neighbouring voices, and semitone clusters. The 150 cheapest candidates per chord go
//! into a dynamic program whose step cost is the total motion (leaps past a fifth count double), common
//! tones that could have stayed, voices passing a neighbour's old note, and parallel fifths and octaves
//! (outer voices count double; with a `bass` given, the voices also avoid parallels with it and stay above
//! it).

use crate::error::{Error, Result};
use crate::theory::chord::{Chord, Role};
use crate::theory::note::note_name;

/// Options for [`voice_lead`]. The defaults are score.py's: 4 voices in C3-G5.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceLeadOptions {
    pub voices: usize,
    pub low: u8,
    pub high: u8,
    /// The voicing to lead from (one pitch per voice, any order), e.g. the last chord before the passage.
    pub start: Option<Vec<u8>>,
    /// The bass pitch under each chord (e.g. from [`bass_line`]): the voices stay above it and avoid
    /// parallels with it. One per chord.
    pub bass: Option<Vec<u8>>,
}

impl Default for VoiceLeadOptions {
    fn default() -> VoiceLeadOptions {
        VoiceLeadOptions { voices: 4, low: 48, high: 79, start: None, bass: None }
    }
}

/// The result of [`voice_lead`]: one ascending voicing per chord, and warnings (parallel fifths/octaves
/// between the outer voices, chords with no voicing above the given bass).
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceLeading {
    pub pitches: Vec<Vec<u8>>,
    pub warnings: Vec<String>,
    /// For each warning, the 0-based index of the chord it is about (the second chord of a pair).
    pub warning_chords: Vec<usize>,
}

/// Options for [`bass_line`]. The defaults are score.py's: C2-G3, inversions on.
#[derive(Clone, Debug, PartialEq)]
pub struct BassLineOptions {
    pub low: u8,
    pub high: u8,
    /// The bass note before the passage, to lead from.
    pub start: Option<u8>,
    /// Play the slash bass of inversions (C/E -> E); off = always the root.
    pub inversions: bool,
}

impl Default for BassLineOptions {
    fn default() -> BassLineOptions {
        BassLineOptions { low: 36, high: 55, start: None, inversions: true }
    }
}

/// Most voicing combinations examined for one chord before giving up (a 4-voice chord over three octaves
/// has about ten thousand).
const MAX_COMBINATIONS: u128 = 5_000_000;
/// Candidate voicings kept per chord.
const CAP: usize = 150;

/// What a thin voicing keeps first (score.py's PRIORITY).
fn priority(r: Role) -> usize {
    match r {
        Role::Third => 0,
        Role::Sus => 1,
        Role::Seventh => 2,
        Role::Root => 3,
        Role::Extension => 4,
        Role::Fifth => 5,
    }
}

/// The cost of doubling a tone (score.py's DOUBLE).
fn double_cost(r: Role) -> f64 {
    match r {
        Role::Root => 0.0,
        Role::Fifth => 0.5,
        Role::Third => 1.5,
        Role::Sus => 2.0,
        Role::Extension => 4.0,
        Role::Seventh => 6.0,
    }
}

/// A major chord over its own third (C/E): the bass has the third, so the voices leave it out.
fn first_inversion(c: &Chord) -> bool {
    c.is_major() && c.third() == Some(c.bass) && c.bass != c.root
}

/// The pitch classes every voicing of `c` with `voices` voices must contain (`skip_third`: the bass has it).
fn need(c: &Chord, voices: usize, skip_third: bool) -> Vec<u8> {
    let mut tones: Vec<&crate::theory::chord::ChordTone> =
        c.tones.iter().filter(|t| !(skip_third && Some(t.pc) == c.third())).collect();
    tones.sort_by_key(|t| priority(t.role));
    tones.iter().take(voices).map(|t| t.pc).collect()
}

fn binomial(n: usize, k: usize) -> u128 {
    if k > n {
        return 0;
    }
    let k = k.min(n - k);
    let mut r: u128 = 1;
    for i in 0..k {
        r = r.saturating_mul((n - i) as u128) / (i as u128 + 1);
        if r > MAX_COMBINATIONS * 1000 {
            return r;
        }
    }
    r
}

fn voicings(c: &Chord, voices: usize, low: u8, high: u8) -> Result<Vec<(f64, Vec<u8>)>> {
    let mut out = candidates(c, voices, low, high, first_inversion(c))?;
    if out.is_empty() && first_inversion(c) {
        // too narrow a range to leave the third to the bass (F/A in G3-E5 has only three Fs and Cs): the voices
        // take it once rather than give up
        out = candidates(c, voices, low, high, false)?;
    }
    if out.is_empty() {
        return Err(Error::Voicing(format!("no {voices}-voice voicing of {} fits in {}-{}", c.symbol, note_name(low), note_name(high))));
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    out.truncate(CAP);
    Ok(out)
}

/// Every voicing of `c` in `low..=high` with its cost, unsorted; `no_third`: leave the third out (to the bass).
fn candidates(c: &Chord, voices: usize, low: u8, high: u8, no_third: bool) -> Result<Vec<(f64, Vec<u8>)>> {
    let need = need(c, voices, no_third);
    let third = c.third();
    let pool: Vec<u8> = (low..=high).filter(|p| c.tone(p % 12).is_some()).collect();
    let combos = binomial(pool.len(), voices);
    if combos > MAX_COMBINATIONS {
        return Err(Error::Voicing(format!(
            "{voices} voices over {} chord tones in {}-{} is {combos} combinations to try for {}; narrow the range or use fewer voices",
            pool.len(),
            note_name(low),
            note_name(high),
            c.symbol
        )));
    }
    let mid = (low as f64 + high as f64) / 2.0;
    let mut out: Vec<(f64, Vec<u8>)> = Vec::new();
    if voices <= pool.len() {
        let mut idx: Vec<usize> = (0..voices).collect();
        let mut v: Vec<u8> = vec![0; voices];
        loop {
            for (slot, &i) in v.iter_mut().zip(&idx) {
                *slot = pool[i];
            }
            if let Some(cost) = voicing_cost(c, &v, &need, third, no_third) {
                let sum: u32 = v.iter().map(|&p| p as u32).sum();
                out.push((cost + 0.08 * (sum as f64 / voices as f64 - mid).abs(), v.clone()));
            }
            // next combination in lexicographic order
            let mut k = voices;
            while k > 0 && idx[k - 1] == pool.len() - voices + k - 1 {
                k -= 1;
            }
            if k == 0 {
                break;
            }
            idx[k - 1] += 1;
            for j in k..voices {
                idx[j] = idx[j - 1] + 1;
            }
        }
    }
    Ok(out)
}

/// The doubling and spacing cost of an ascending voicing, or None if it misses a needed tone or doubles
/// the third of a major chord.
fn voicing_cost(c: &Chord, v: &[u8], need: &[u8], third: Option<u8>, no_third: bool) -> Option<f64> {
    let mut counts = [0usize; 12];
    for &p in v {
        counts[(p % 12) as usize] += 1;
    }
    if need.iter().any(|&pc| counts[pc as usize] == 0) {
        return None;
    }
    if c.is_major()
        && let Some(t) = third
        && counts[t as usize] > if no_third { 0 } else { 1 }
    {
        return None;
    }
    let mut cost = 0.0;
    for t in &c.tones {
        let n = counts[t.pc as usize];
        if n > 1 {
            cost += double_cost(t.role) * (n - 1) as f64;
        }
    }
    for w in v.windows(2) {
        let d = w[1] as i32 - w[0] as i32;
        if d > 12 {
            cost += 3.0 + 0.3 * (d - 12) as f64;
        } else if d == 1 {
            cost += 1.5;
        }
    }
    Some(cost)
}

/// "octaves" or "fifths" if two voices (a below, b above) move in parallel perfect intervals.
fn parallel(a0: i32, a1: i32, b0: i32, b1: i32) -> Option<&'static str> {
    let (i0, i1) = ((b0 - a0).rem_euclid(12), (b1 - a1).rem_euclid(12));
    if i0 == i1 && (i0 == 0 || i0 == 7) && a0 != a1 && (a1 - a0) * (b1 - b0) > 0 {
        Some(if i0 == 0 { "octaves" } else { "fifths" })
    } else {
        None
    }
}

/// The cost of moving voicing `a` to `b` over chord `next` (`bass` = the old and new bass under them).
fn move_cost(a: &[u8], b: &[u8], next: &Chord, bass: Option<(u8, u8)>) -> f64 {
    let (a, b): (Vec<i32>, Vec<i32>) = (a.iter().map(|&x| x as i32).collect(), b.iter().map(|&x| x as i32).collect());
    let mut cost = 0.0;
    for i in 0..a.len() {
        let (x, y) = (a[i], b[i]);
        let d = (x - y).abs();
        cost += (d + (d - 7).max(0)) as f64; // leaps past a fifth count double
        if d != 0 && next.tone((x % 12) as u8).is_some() {
            cost += 1.5; // a common tone that could have stayed
        }
        if i + 1 < a.len() && (y > a[i + 1] || b[i + 1] < x) {
            cost += 3.0; // overlap: a voice passes its neighbour's old note
        }
    }
    let top = a.len() - 1;
    for i in 0..a.len() {
        for j in i + 1..a.len() {
            if parallel(a[i], b[i], a[j], b[j]).is_some() {
                cost += if i == 0 && j == top && bass.is_none() { 8.0 } else { 4.0 };
            }
        }
    }
    if let Some((b0, b1)) = bass {
        for j in 0..a.len() {
            if parallel(b0 as i32, b1 as i32, a[j], b[j]).is_some() {
                cost += if j == top { 8.0 } else { 4.0 };
            }
        }
    }
    cost
}

fn check_range(low: u8, high: u8) -> Result<()> {
    if low > high || high > 127 {
        return Err(Error::invalid(format!("range {}-{} is empty or outside MIDI 0-127", low, high)));
    }
    Ok(())
}

/// Voices a chord progression with the least total movement (see the module docs).
pub fn voice_lead(chords: &[Chord], opts: &VoiceLeadOptions) -> Result<VoiceLeading> {
    let voices = opts.voices;
    if !(1..=12).contains(&voices) {
        return Err(Error::invalid(format!("voices must be 1-12, got {voices}")));
    }
    check_range(opts.low, opts.high)?;
    if let Some(bass) = &opts.bass
        && bass.len() != chords.len()
    {
        return Err(Error::invalid(format!("bass has {} pitches for {} chords (one per chord)", bass.len(), chords.len())));
    }
    if let Some(s) = &opts.start
        && s.len() != voices
    {
        return Err(Error::invalid(format!("start has {} pitches; it needs one per voice ({voices})", s.len())));
    }
    if chords.is_empty() {
        return Ok(VoiceLeading { pitches: vec![], warnings: vec![], warning_chords: vec![] });
    }
    let bass = opts.bass.as_deref();
    let mut warnings = Vec::new();
    let mut warning_chords = Vec::new();
    let mut cands: Vec<Vec<(f64, Vec<u8>)>> = Vec::with_capacity(chords.len());
    for (k, c) in chords.iter().enumerate() {
        let vs = voicings(c, voices, opts.low, opts.high)?;
        let vs = match bass {
            Some(b) => {
                let above: Vec<(f64, Vec<u8>)> = vs.iter().filter(|x| x.1[0] > b[k]).cloned().collect();
                if above.is_empty() {
                    warnings.push(format!("chord {} ({}): no voicing above the bass {}", k + 1, c.symbol, note_name(b[k])));
                    warning_chords.push(k);
                    vs
                } else {
                    above
                }
            }
            None => vs,
        };
        cands.push(vs);
    }
    let start: Option<Vec<u8>> = opts.start.as_ref().map(|s| {
        let mut s = s.clone();
        s.sort_unstable();
        s
    });
    // back[k][i] = (best total cost ending in candidate i of chord k, index of its predecessor)
    let mut back: Vec<Vec<(f64, usize)>> = Vec::with_capacity(chords.len());
    back.push(
        cands[0]
            .iter()
            .map(|(cost, v)| (cost + start.as_ref().map_or(0.0, |s| move_cost(s, v, &chords[0], None)), 0))
            .collect(),
    );
    for k in 1..chords.len() {
        let pair = bass.map(|b| (b[k - 1], b[k]));
        let prev = &back[k - 1];
        let row: Vec<(f64, usize)> = cands[k]
            .iter()
            .map(|(cost, v)| {
                let mut best = (f64::INFINITY, 0usize);
                for (i, (_, u)) in cands[k - 1].iter().enumerate() {
                    let total = prev[i].0 + move_cost(u, v, &chords[k], pair);
                    if total < best.0 {
                        best = (total, i);
                    }
                }
                (cost + best.0, best.1)
            })
            .collect();
        back.push(row);
    }
    let last = &back[chords.len() - 1];
    let mut i = (0..last.len()).fold(0, |m, j| if last[j].0 < last[m].0 { j } else { m });
    let mut path: Vec<Vec<u8>> = Vec::with_capacity(chords.len());
    for k in (0..chords.len()).rev() {
        path.push(cands[k][i].1.clone());
        i = back[k][i].1;
    }
    path.reverse();
    for k in 1..path.len() {
        let (lo0, lo1) = match bass {
            Some(b) => (b[k - 1], b[k]),
            None => (path[k - 1][0], path[k][0]),
        };
        let (hi0, hi1) = (path[k - 1][voices - 1], path[k][voices - 1]);
        if let Some(kind) = parallel(lo0 as i32, lo1 as i32, hi0 as i32, hi1 as i32) {
            warnings.push(format!(
                "chord {}->{} ({} -> {}): parallel {kind} between outer voices ({}-{} -> {}-{})",
                k,
                k + 1,
                chords[k - 1].symbol,
                chords[k].symbol,
                note_name(lo0),
                note_name(hi0),
                note_name(lo1),
                note_name(hi1)
            ));
            warning_chords.push(k);
        }
    }
    Ok(VoiceLeading { pitches: path, warnings, warning_chords })
}

/// One bass pitch per chord: the slash bass of inversions (else the root), in the octave that moves least
/// (leaps past a fifth cost double), leaning to the lower third of the range. score.py's `bass_line`.
pub fn bass_line(chords: &[Chord], opts: &BassLineOptions) -> Result<Vec<u8>> {
    check_range(opts.low, opts.high)?;
    if chords.is_empty() {
        return Ok(vec![]);
    }
    let (low, high) = (opts.low as i32, opts.high as i32);
    let home = low as f64 + (high - low) as f64 / 3.0;
    let mut cands: Vec<Vec<i32>> = Vec::with_capacity(chords.len());
    for c in chords {
        let pc = if opts.inversions { c.bass } else { c.root } as i32;
        let ps: Vec<i32> = (low..=high).filter(|p| p % 12 == pc).collect();
        if ps.is_empty() {
            return Err(Error::Voicing(format!(
                "no {} for {} in {}-{}",
                crate::theory::note::pc_name(pc as u8, false),
                c.symbol,
                note_name(opts.low),
                note_name(opts.high)
            )));
        }
        cands.push(ps);
    }
    let leap = |a: i32, b: i32| ((a - b).abs() + ((a - b).abs() - 7).max(0)) as f64;
    let mut row: Vec<f64> =
        cands[0].iter().map(|&p| opts.start.map_or(0.0, |s| leap(s as i32, p)) + 0.1 * (p as f64 - home).abs()).collect();
    let mut back: Vec<Vec<usize>> = Vec::new();
    for k in 1..chords.len() {
        let mut next = Vec::with_capacity(cands[k].len());
        let mut ptr = Vec::with_capacity(cands[k].len());
        for &p in &cands[k] {
            let j = (0..row.len()).fold(0, |m, i| if row[i] + leap(cands[k - 1][i], p) < row[m] + leap(cands[k - 1][m], p) { i } else { m });
            next.push(row[j] + leap(cands[k - 1][j], p) + 0.1 * (p as f64 - home).abs());
            ptr.push(j);
        }
        row = next;
        back.push(ptr);
    }
    let mut i = (0..row.len()).fold(0, |m, j| if row[j] < row[m] { j } else { m });
    let mut out = vec![cands[chords.len() - 1][i] as u8];
    for k in (0..chords.len() - 1).rev() {
        i = back[k][i];
        out.push(cands[k][i] as u8);
    }
    out.reverse();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chords(s: &[&str]) -> Vec<Chord> {
        s.iter().map(|c| Chord::parse(c).unwrap()).collect()
    }

    #[test]
    fn voices_stay_in_range_and_hold_the_chord() {
        let cs = chords(&["G", "Em", "C", "Dsus4", "D"]);
        let vl = voice_lead(&cs, &VoiceLeadOptions { low: 55, high: 79, ..Default::default() }).unwrap();
        assert_eq!(vl.pitches.len(), 5);
        for (v, c) in vl.pitches.iter().zip(&cs) {
            assert_eq!(v.len(), 4);
            assert!(v.windows(2).all(|w| w[0] <= w[1]));
            assert!(v.iter().all(|&p| (55..=79).contains(&p) && c.tone(p % 12).is_some()));
            // a major third is never doubled
            if let Some(t) = c.third().filter(|_| c.is_major()) {
                assert!(v.iter().filter(|&&p| p % 12 == t).count() <= 1);
            }
        }
    }

    #[test]
    fn first_inversion_leaves_the_third_to_the_bass() {
        let cs = chords(&["C/E"]);
        let vl = voice_lead(&cs, &VoiceLeadOptions::default()).unwrap();
        assert!(vl.pitches[0].iter().all(|p| p % 12 != 4));
    }

    #[test]
    fn bass_lines_follow_inversions() {
        let cs = chords(&["C", "C/E", "F", "G"]);
        let b = bass_line(&cs, &BassLineOptions::default()).unwrap();
        assert_eq!(b.iter().map(|p| p % 12).collect::<Vec<_>>(), [0, 4, 5, 7]);
        let b = bass_line(&cs, &BassLineOptions { inversions: false, ..Default::default() }).unwrap();
        assert_eq!(b.iter().map(|p| p % 12).collect::<Vec<_>>(), [0, 0, 5, 7]);
        assert!(b.iter().all(|p| (36..=55).contains(p)));
    }

    #[test]
    fn a_first_inversion_in_a_narrow_range_takes_its_third_rather_than_fail() {
        // F/A in G3-E5: the Fs and Cs alone (C4 F4 C5) can't fill four voices
        let vl = voice_lead(&chords(&["F/A"]), &VoiceLeadOptions { low: 55, high: 76, ..Default::default() }).unwrap();
        let v = &vl.pitches[0];
        assert_eq!(v.len(), 4);
        assert!(v.iter().all(|p| (55..=76).contains(p)));
        assert_eq!(v.iter().filter(|p| *p % 12 == 9).count(), 1, "the third once: {v:?}");
        // with room, the voices still leave it to the bass
        let vl = voice_lead(&chords(&["F/A"]), &VoiceLeadOptions::default()).unwrap();
        assert!(vl.pitches[0].iter().all(|p| p % 12 != 9), "{:?}", vl.pitches[0]);
    }

    #[test]
    fn errors_and_warnings() {
        let cs = chords(&["C"]);
        assert!(voice_lead(&cs, &VoiceLeadOptions { low: 60, high: 62, ..Default::default() }).unwrap_err().to_string().contains("no 4-voice voicing of C fits in C4-D4"));
        assert!(voice_lead(&cs, &VoiceLeadOptions { voices: 0, ..Default::default() }).is_err());
        assert!(voice_lead(&cs, &VoiceLeadOptions { start: Some(vec![60]), ..Default::default() }).is_err());
        assert!(voice_lead(&cs, &VoiceLeadOptions { bass: Some(vec![]), ..Default::default() }).is_err());
        assert!(bass_line(&chords(&["C#"]), &BassLineOptions { low: 40, high: 48, ..Default::default() }).unwrap_err().to_string().contains("no C# for C#"));
        let vl = voice_lead(&cs, &VoiceLeadOptions { bass: Some(vec![80]), ..Default::default() }).unwrap();
        assert_eq!(vl.warnings, ["chord 1 (C): no voicing above the bass G#5"]);
        assert!(voice_lead(&[], &VoiceLeadOptions::default()).unwrap().pitches.is_empty());
        let wide = voice_lead(&cs, &VoiceLeadOptions { voices: 12, low: 0, high: 127, ..Default::default() });
        assert!(wide.unwrap_err().to_string().contains("combinations"));
    }

    #[test]
    fn parallels_are_detected() {
        assert_eq!(parallel(48, 50, 55, 57), Some("fifths"));
        assert_eq!(parallel(48, 50, 60, 62), Some("octaves"));
        assert_eq!(parallel(48, 48, 55, 55), None);
        assert_eq!(parallel(48, 50, 55, 53), None);
    }
}
