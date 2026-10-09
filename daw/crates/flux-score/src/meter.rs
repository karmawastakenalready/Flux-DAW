//! Time signatures and metric weight: how strong each position in a bar is.

use crate::error::{Error, Result};
use crate::util::round_even;
use flux_core::Project;
use std::fmt;
use std::str::FromStr;

/// A time signature. Beats are quarter notes throughout Flux, so a 6/8 bar is 3 beats long.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Meter {
    pub num: u32,
    pub den: u32,
}

impl Meter {
    /// A meter with a numerator of 1-64 and a power-of-two denominator from 1 to 64.
    pub fn new(num: u32, den: u32) -> Result<Meter> {
        if !(1..=64).contains(&num) {
            return Err(Error::invalid(format!("time signature {num}/{den}: the numerator must be 1-64")));
        }
        if !den.is_power_of_two() || den > 64 {
            return Err(Error::invalid(format!("time signature {num}/{den}: the denominator must be 1, 2, 4, 8, 16, 32 or 64")));
        }
        Ok(Meter { num, den })
    }

    /// The project's time signature.
    pub fn of(project: &Project) -> Result<Meter> {
        Meter::new(project.time_signature.0, project.time_signature.1)
    }

    /// Parses "6/8", "4/4", "12/8".
    pub fn parse(text: &str) -> Result<Meter> {
        let bad = || Error::invalid(format!("time signature {text:?}: write it as numerator/denominator, e.g. 4/4 or 6/8"));
        let (a, b) = text.trim().split_once('/').ok_or_else(bad)?;
        let num = a.trim().parse().map_err(|_| bad())?;
        let den = b.trim().parse().map_err(|_| bad())?;
        Meter::new(num, den)
    }

    /// Quarter-note beats per bar: 4/4 = 4, 6/8 = 3, 7/8 = 3.5.
    pub fn bar_beats(&self) -> f64 {
        self.num as f64 * 4.0 / self.den as f64
    }

    /// Compound meters (6/8, 9/8, 12/8) count dotted quarters.
    pub fn is_compound(&self) -> bool {
        self.den == 8 && self.num.is_multiple_of(3) && self.num > 3
    }

    /// The counted beat in quarter notes: 1.5 in compound meters, else one denominator note.
    pub fn beat_unit(&self) -> f64 {
        if self.is_compound() { 1.5 } else { 4.0 / self.den as f64 }
    }

    /// The metric weight of an absolute beat (bar 1 starts at 0): 1 on the downbeat, 0.6 on the
    /// other strong beat of an even number of counted beats (beat 3 of 4/4, the 4th eighth of 6/8, the 7th
    /// of 12/8), 0.3 on the other counted beats, 0.12 on their subdivisions (eighths in simple meters, the
    /// eighths of compound ones) and 0 elsewhere. Positions within 0.03 beat of a grid point count as on
    /// it. Identical to score.py's `metric_weight`.
    pub fn weight(&self, beat: f64) -> f64 {
        let bar = self.bar_beats();
        let compound = self.is_compound();
        let unit = self.beat_unit();
        let pos = beat.rem_euclid(bar);
        let on = |g: f64| (pos / g - round_even(pos / g)).abs() * g < 0.03;
        let beats = round_even(bar / unit) as i64;
        if on(bar) {
            1.0
        } else if beats % 2 == 0 && on(bar / 2.0) {
            0.6
        } else if on(unit) {
            0.3
        } else if on(unit / if compound { 3.0 } else { 2.0 }) {
            0.12
        } else {
            0.0
        }
    }
}

impl Default for Meter {
    fn default() -> Meter {
        Meter { num: 4, den: 4 }
    }
}

impl FromStr for Meter {
    type Err = Error;
    fn from_str(s: &str) -> Result<Meter> {
        Meter::parse(s)
    }
}

impl fmt::Display for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// score.py's `metric_weight(beat, meter)`.
pub fn metric_weight(beat: f64, meter: Meter) -> f64 {
    meter.weight(beat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_in_simple_and_compound_meters() {
        let m44 = Meter::new(4, 4).unwrap();
        assert_eq!([0.0, 1.0, 2.0, 3.0, 0.5, 0.25, 4.0, 6.0].map(|b| m44.weight(b)), [1.0, 0.3, 0.6, 0.3, 0.12, 0.0, 1.0, 0.6]);
        let m68 = Meter::parse("6/8").unwrap();
        assert!(m68.is_compound());
        assert_eq!(m68.bar_beats(), 3.0);
        assert_eq!([0.0, 1.5, 0.5, 1.0, 0.25, 2.99].map(|b| m68.weight(b)), [1.0, 0.6, 0.12, 0.12, 0.0, 1.0]);
        let m34 = Meter::new(3, 4).unwrap();
        assert_eq!([0.0, 1.0, 2.0, 1.5].map(|b| m34.weight(b)), [1.0, 0.3, 0.3, 0.12]);
        let m128 = Meter::new(12, 8).unwrap();
        assert_eq!([0.0, 3.0, 1.5, 4.5, 0.5].map(|b| m128.weight(b)), [1.0, 0.6, 0.3, 0.3, 0.12]);
        assert_eq!(m44.weight(-1.0), 0.3);
    }

    #[test]
    fn rejects_bad_signatures() {
        assert!(Meter::new(0, 4).is_err());
        assert!(Meter::new(4, 3).is_err());
        assert!(Meter::parse("4-4").is_err());
        assert_eq!(Meter::parse(" 7/8 ").unwrap().bar_beats(), 3.5);
        assert_eq!(Meter::default().to_string(), "4/4");
    }
}
