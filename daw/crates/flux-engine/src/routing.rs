//! How the tracks feed each other: group buses (a track's `output`), aux sends, and what solo and mute leave
//! sounding. The offline render and live playback both work from it, so they route alike.

use flux_core::Project;

/// Where each track's signal goes and which tracks sound, by track index.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Routing {
    /// Track indices, each after every track that feeds it.
    pub order: Vec<usize>,
    /// The track each one's output goes to (None = the master).
    pub output: Vec<Option<usize>>,
    /// Each track's aux sends: (receiving track, level 0..1).
    pub sends: Vec<Vec<(usize, f64)>>,
    /// The tracks feeding each one, through their outputs or sends.
    pub inputs: Vec<Vec<usize>>,
    /// Its own instrument sounds (it isn't muted, or solo leaves it in, and its signal reaches the master).
    pub source: Vec<bool>,
    /// Its strip runs: its own instrument or something that feeds it sounds, and its signal reaches the master.
    pub pass: Vec<bool>,
}

impl Routing {
    pub fn of(p: &Project) -> Routing {
        let n = p.tracks.len();
        let index = |id: u64| p.tracks.iter().position(|t| t.id == id);
        let mut output: Vec<Option<usize>> = p.tracks.iter().map(|t| t.mixer.output.and_then(index)).collect();
        let mut sends: Vec<Vec<(usize, f64)>> = p
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| t.mixer.sends.iter().filter_map(|s| index(s.to).filter(|&j| j != i).map(|j| (j, s.level.clamp(0.0, 1.0)))).collect())
            .collect();
        for (i, o) in output.iter_mut().enumerate() {
            if *o == Some(i) {
                *o = None;
            }
        }
        // a hand-edited file can hold a loop: the tracks in one keep their sound, going straight to the master
        let order = loop {
            match topological(n, &output, &sends) {
                Ok(order) => break order,
                Err(stuck) => {
                    for i in stuck {
                        output[i] = None;
                        sends[i].clear();
                    }
                }
            }
        };
        let mut inputs = vec![vec![]; n];
        for i in 0..n {
            for j in output[i].into_iter().chain(sends[i].iter().map(|s| s.0)) {
                if !inputs[j].contains(&i) {
                    inputs[j].push(i);
                }
            }
        }

        let solo: Vec<bool> = p.tracks.iter().map(|t| t.mixer.solo).collect();
        let mute: Vec<bool> = p.tracks.iter().map(|t| t.mixer.mute).collect();
        let any_solo = solo.iter().any(|&s| s);
        // soloing a bus solos what's routed into it
        let in_soloed_group = |mut i: usize| loop {
            if solo[i] {
                break true;
            }
            match output[i] {
                Some(o) => i = o,
                None => break false,
            }
        };
        let src_ok: Vec<bool> =
            (0..n).map(|i| if any_solo { solo[i] || (in_soloed_group(i) && !mute[i]) } else { !mute[i] }).collect();
        let node_ok: Vec<bool> = (0..n).map(|i| solo[i] || !mute[i]).collect();
        // forward: what carries sound
        let mut live = vec![false; n];
        for &i in &order {
            live[i] = node_ok[i] && (src_ok[i] || inputs[i].iter().any(|&j| live[j]));
        }
        // backward: what reaches the master
        let mut reach = vec![false; n];
        for &i in order.iter().rev() {
            let onward = match output[i] {
                None => true,
                Some(o) => reach[o],
            };
            reach[i] = live[i] && (onward || sends[i].iter().any(|s| reach[s.0]));
        }
        let source = (0..n).map(|i| src_ok[i] && reach[i]).collect();
        Routing { order, output, sends, inputs, source, pass: reach }
    }

    /// Whether other tracks feed this one.
    pub fn is_bus(&self, i: usize) -> bool {
        !self.inputs[i].is_empty()
    }

    /// Whether any track feeds another.
    pub fn has_buses(&self) -> bool {
        self.inputs.iter().any(|v| !v.is_empty())
    }

    /// Whether the track sounds in the master at all.
    pub fn audible(&self, i: usize) -> bool {
        self.pass[i]
    }
}

/// Track indices with every track after those feeding it, or the tracks caught in loops.
fn topological(n: usize, output: &[Option<usize>], sends: &[Vec<(usize, f64)>]) -> Result<Vec<usize>, Vec<usize>> {
    let mut indeg = vec![0usize; n];
    let next = |i: usize| output[i].into_iter().chain(sends[i].iter().map(|s| s.0));
    for i in 0..n {
        for j in next(i) {
            indeg[j] += 1;
        }
    }
    // in track order where the routing allows
    let mut ready: Vec<usize> = (0..n).rev().filter(|&i| indeg[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = ready.pop() {
        order.push(i);
        for j in next(i) {
            indeg[j] -= 1;
            if indeg[j] == 0 {
                ready.push(j);
                ready.sort_unstable_by(|a, b| b.cmp(a));
            }
        }
    }
    if order.len() == n { Ok(order) } else { Err((0..n).filter(|&i| indeg[i] > 0).collect()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{AuxSend, Instrument, Project};

    fn project(names: &[&str]) -> Project {
        let mut p = Project::new("t", 120.0, 4);
        for n in names {
            flux_core::commands::apply(&mut p, &flux_core::Command::AddTrack { name: n.to_string(), instrument: Instrument::None, color: None })
                .unwrap();
        }
        p
    }

    #[test]
    fn buses_come_after_what_feeds_them() {
        let mut p = project(&["Bus", "Kick", "Snare", "Verb"]);
        let (bus, verb) = (p.tracks[0].id, p.tracks[3].id);
        p.tracks[1].mixer.output = Some(bus);
        p.tracks[2].mixer.output = Some(bus);
        p.tracks[0].mixer.sends = vec![AuxSend { to: verb, level: 0.3 }];
        let r = Routing::of(&p);
        let at = |i: usize| r.order.iter().position(|&x| x == i).unwrap();
        assert!(at(1) < at(0) && at(2) < at(0) && at(0) < at(3));
        assert_eq!(r.inputs[0], vec![1, 2]);
        assert_eq!(r.inputs[3], vec![0]);
        assert!(r.pass.iter().all(|&x| x));
    }

    #[test]
    fn soloing_a_bus_solos_its_tracks_and_soloing_a_track_keeps_its_buses() {
        let mut p = project(&["Bus", "Kick", "Pad", "Verb"]);
        let (bus, verb) = (p.tracks[0].id, p.tracks[3].id);
        p.tracks[1].mixer.output = Some(bus);
        p.tracks[1].mixer.sends = vec![AuxSend { to: verb, level: 0.5 }];
        p.tracks[0].mixer.solo = true;
        let r = Routing::of(&p);
        assert_eq!(r.source, vec![true, true, false, false]);
        assert_eq!(r.pass, vec![true, true, false, true]);

        p.tracks[0].mixer.solo = false;
        p.tracks[1].mixer.solo = true;
        let r = Routing::of(&p);
        assert_eq!(r.source, vec![false, true, false, false]);
        assert_eq!(r.pass, vec![true, true, false, true]);
    }

    #[test]
    fn a_muted_bus_silences_what_goes_through_it() {
        let mut p = project(&["Bus", "Kick", "Pad"]);
        let bus = p.tracks[0].id;
        p.tracks[1].mixer.output = Some(bus);
        p.tracks[0].mixer.mute = true;
        let r = Routing::of(&p);
        assert_eq!(r.source, vec![false, false, true]);
        assert_eq!(r.pass, vec![false, false, true]);
    }

    #[test]
    fn a_loop_in_a_file_goes_straight_to_the_master() {
        let mut p = project(&["A", "B"]);
        let (a, b) = (p.tracks[0].id, p.tracks[1].id);
        p.tracks[0].mixer.output = Some(b);
        p.tracks[1].mixer.output = Some(a);
        let r = Routing::of(&p);
        assert_eq!(r.output, vec![None, None]);
        assert_eq!(r.order.len(), 2);
    }
}
