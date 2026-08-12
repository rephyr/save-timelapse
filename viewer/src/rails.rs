//! How long a rail piece is and which way it runs.
//!
//! A rail is recorded as a 1x1 entity like everything else, and drawing it as
//! a 1x1 square is why track used to look like a dotted line: a straight rail
//! spans two tiles and the pieces sit two tiles apart, so half of every run
//! went unpainted. A diagonal was worse, painting a staircase of squares along
//! a line that is not axis aligned, and a corner worst of all, being where the
//! most piece types meet.
//!
//! So each piece is drawn as a segment along the path it actually occupies.
//!
//! # Where these numbers come from
//!
//! Measured, not assumed. Factorio does not expose rail geometry: the
//! prototype definitions say as much (`collision box is hardcoded for rails as
//! to avoid unexpected changes in the way rail blocks are merged`), so there
//! is nothing to read off disk. What a real capture does show is the step
//! between consecutive pieces in a straight run, and that step is one piece.
//!
//! Directions eight apart are the same segment, a segment having no sense of
//! which end is which, which is why the table pairs them.
//!
//! Curves are the exception, and cannot be measured that way at all: parallel
//! track two tiles away puts endpoints exactly where a real joint would be.
//! For those the mod records which rails the game says are connected, and
//! [`solve`] turns that into geometry, a straight rail's ends being known.

use std::collections::HashMap;

/// One rail piece as a straight segment through its own centre.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RailSegment {
    /// Along the run, in tiles.
    pub length: f32,
    /// Radians, the angle of the run in screen space, which is what
    /// `draw_rectangle_ex` takes. Factorio's y axis points down and so does
    /// the screen's, so no flip is needed between them.
    pub rotation: f32,
    /// Where the middle of the run sits relative to the piece's recorded
    /// position, in tiles. Zero for a straight, whose ends sit either side of
    /// it; a curve half is not centred on itself.
    pub offset: (f32, f32),
}

impl RailSegment {
    /// The two ends, relative to the piece's recorded position.
    pub fn ends(&self) -> [(f32, f32); 2] {
        let (half_x, half_y) = (self.rotation.cos() * self.length / 2.0, self.rotation.sin() * self.length / 2.0);
        [(self.offset.0 - half_x, self.offset.1 - half_y), (self.offset.0 + half_x, self.offset.1 + half_y)]
    }

    /// The piece running between these two points, both relative to its own
    /// position.
    fn between(a: (f32, f32), b: (f32, f32)) -> RailSegment {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        RailSegment { length: (dx * dx + dy * dy).sqrt(), rotation: dy.atan2(dx), offset: ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0) }
    }
}

/// How wide to draw track, in tiles. Narrower than the two rails really span,
/// so that the four-tile spacing of a double track stays legible when zoomed
/// out, which is the range this matters at.
pub const RAIL_WIDTH_TILES: f32 = 0.6;

/// Every prototype whose geometry this knows. Curves are absent on purpose:
/// their shape could not be determined from a capture, parallel track a couple
/// of tiles away polluting every neighbour analysis, and guessing would be
/// wrong in a way that is harder to spot than a square. They keep the square
/// until the mod reports their real endpoints.
pub fn rail_segment(name: &str, direction: u8) -> Option<RailSegment> {
    let (dx, dy): (f32, f32) = match name {
        // Two tiles long. Measured directly: consecutive pieces in a run sit
        // exactly two tiles apart, on both the cardinal and diagonal facings.
        "straight-rail" | "elevated-straight-rail" => match direction % 8 {
            0 => (0.0, 2.0),
            2 => (2.0, -2.0),
            4 => (2.0, 0.0),
            6 => (2.0, 2.0),
            _ => return None,
        },
        // 1.0's rails, kept by the game for maps built before 2.0 and still
        // laid by some mods. Cardinals match, but the two diagonal facings are
        // the other way round, so sharing the table above drew every legacy
        // diagonal across the track it sits on rather than along it.
        "legacy-straight-rail" => match direction % 8 {
            0 => (0.0, 2.0),
            2 => (2.0, 2.0),
            4 => (2.0, 0.0),
            6 => (2.0, -2.0),
            _ => return None,
        },
        // The 1:2 slope that joins a cardinal run to a diagonal one, spanning
        // four tiles one way and two the other. Facings 4 and 6 were measured;
        // the other two are those turned a quarter turn, four direction steps
        // being ninety degrees.
        "half-diagonal-rail" | "elevated-half-diagonal-rail" => match direction % 8 {
            0 => (2.0, 4.0),
            2 => (2.0, -4.0),
            4 => (4.0, -2.0),
            6 => (4.0, 2.0),
            _ => return None,
        },
        // 1.0's corner: one prototype for a whole quarter turn, where 2.0
        // splits the same turn into two halves. Drawn as the chord between its
        // ends, which meets its neighbours exactly and reads as a corner, where
        // the square it drew before read as a gap.
        //
        // The ends were measured against legacy straights, whose geometry is
        // known: a joint shared by two straights is counted twice over the
        // pieces sampled, and a curve's own end only once, which is what tells
        // them apart. Turning any facing a quarter turn lands on the next,
        // and every piece comes out the same length, so the eight agree.
        "legacy-curved-rail" => {
            let (a, b) = match direction % 8 {
                0 => ((1.0, 4.0), (-2.0, -2.0)),
                2 => ((-1.0, 4.0), (2.0, -2.0)),
                4 => ((-4.0, 1.0), (2.0, -2.0)),
                6 => ((-4.0, -1.0), (2.0, 2.0)),
                _ => return None,
            };
            // Facings eight apart are the same corner turned about its own
            // position, so the ends simply negate.
            let flip = direction >= 8;
            let turn = |(x, y): (f32, f32)| if flip { (-x, -y) } else { (x, y) };
            return Some(RailSegment::between(turn(a), turn(b)));
        }
        _ => return None,
    };
    Some(RailSegment { length: (dx * dx + dy * dy).sqrt(), rotation: dy.atan2(dx), offset: (0.0, 0.0) })
}

/// Two joints closer than this are one joint. Rails sit on half-tile
/// positions, so anything genuinely distinct is half a tile apart at least.
const SAME_JOINT: f32 = 0.05;

/// Every rail piece's geometry, worked out from which rails connect to which.
///
/// The mod records connectivity because Factorio states that exactly, where it
/// will not state geometry at all. This turns one into the other: a straight
/// rail's ends are known, so a curve attached to one has a known joint there,
/// and a curve attached to that curve resolves on the round after.
///
/// A piece that never gathers two distinct joints is left out rather than
/// guessed at, which is what a rail with only one thing attached looks like.
/// The caller keeps drawing those the old way.
pub fn solve(samples: &[save_timelapse::prototypes::RailSample]) -> HashMap<(String, u8), RailSegment> {
    let mut known: HashMap<(String, u8), RailSegment> = HashMap::new();
    // Seeded with everything already measured, including the neighbours,
    // which is what the first round has to push out from.
    for sample in samples {
        if let Some(segment) = rail_segment(&sample.name, sample.direction) {
            known.insert((sample.name.clone(), sample.direction), segment);
        }
        for link in &sample.links {
            if let Some(segment) = rail_segment(&link.name, link.direction) {
                known.insert((link.name.clone(), link.direction), segment);
            }
        }
    }

    // A curve half touches a straight on one side and its twin on the other,
    // so the twin needs the round after. Four is slack over the two a vanilla
    // corner takes, and the loop stops as soon as a round learns nothing.
    for _ in 0..4 {
        let mut learned = false;
        for sample in samples {
            let key = (sample.name.clone(), sample.direction);
            if known.contains_key(&key) {
                continue;
            }
            // The neighbour's two ends in our frame; the nearer one is where
            // it meets us, the far one being its other end.
            let joints = joints_from_known(sample, &known);
            // A junction attaches more than two rails, but they meet us at the
            // same two places, so the pair furthest apart is the run itself.
            if let Some((a, b)) = furthest_apart(&joints) {
                known.insert(key, RailSegment::between(a, b));
                learned = true;
            }
        }
        if !learned {
            break;
        }
    }

    pair_up_the_rest(samples, &mut known);
    known
}

/// The joints a piece can work out from neighbours already known.
fn joints_from_known(sample: &save_timelapse::prototypes::RailSample, known: &HashMap<(String, u8), RailSegment>) -> Vec<Joint> {
    let mut joints: Vec<Joint> = Vec::new();
    for link in &sample.links {
        let Some(neighbour) = known.get(&(link.name.clone(), link.direction)) else {
            continue;
        };
        let ends = neighbour.ends().map(|(x, y)| (link.x + x, link.y + y));
        let joint = match distance(ends[0], (0.0, 0.0)) <= distance(ends[1], (0.0, 0.0)) {
            true => ends[0],
            false => ends[1],
        };
        if !joints.iter().any(|&seen| distance(seen, joint) < SAME_JOINT) {
            joints.push(joint);
        }
    }
    joints
}

/// Two unknown pieces that meet each other, each with one end pinned by
/// something known.
///
/// This is what a 2.0 corner is: two halves, one touching a straight and the
/// other a half diagonal, meeting in the middle. Neither ever gathers the two
/// joints the loop above needs, so without this they stay unknown however many
/// rounds it runs, and no amount of recorded connectivity changes that.
///
/// Where they meet cannot be derived, only placed, so it goes midway between
/// the two ends that are known. The pair then draws as two segments across the
/// turn, which is closer to the real arc than one chord would be and much
/// closer than the two squares it replaces.
fn pair_up_the_rest(samples: &[save_timelapse::prototypes::RailSample], known: &mut HashMap<(String, u8), RailSegment>) {
    // Worked out against the same `known` for every pair, so the order samples
    // arrive in cannot change the answer.
    let anchors: HashMap<(String, u8), Joint> = samples
        .iter()
        .filter(|s| !known.contains_key(&(s.name.clone(), s.direction)))
        .filter_map(|s| {
            let joints = joints_from_known(s, known);
            (joints.len() == 1).then(|| ((s.name.clone(), s.direction), joints[0]))
        })
        .collect();

    let mut solved: Vec<((String, u8), RailSegment)> = Vec::new();
    for sample in samples {
        let key = (sample.name.clone(), sample.direction);
        let Some(&mine) = anchors.get(&key) else {
            continue;
        };
        for link in &sample.links {
            let partner = (link.name.clone(), link.direction);
            if partner == key {
                continue;
            }
            let Some(&theirs) = anchors.get(&partner) else {
                continue;
            };
            // Their anchored end, seen from here.
            let theirs = (link.x + theirs.0, link.y + theirs.1);
            let meeting = ((mine.0 + theirs.0) / 2.0, (mine.1 + theirs.1) / 2.0);
            solved.push((key.clone(), RailSegment::between(mine, meeting)));
            break;
        }
    }
    known.extend(solved);
}

/// A point in tiles, relative to whichever rail is being described.
type Joint = (f32, f32);

fn distance(a: Joint, b: Joint) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn furthest_apart(points: &[Joint]) -> Option<(Joint, Joint)> {
    let mut best: Option<(f32, (Joint, Joint))> = None;
    for (i, &a) in points.iter().enumerate() {
        for &b in &points[i + 1..] {
            let span = distance(a, b);
            if best.map(|(far, _)| span > far).unwrap_or(true) {
                best = Some((span, (a, b)));
            }
        }
    }
    best.map(|(_, pair)| pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measurement this table rests on: pieces in a run sit exactly one
    /// piece apart, so a straight rail is two tiles long and not one.
    #[test]
    fn a_straight_rail_spans_the_two_tiles_it_really_occupies() {
        for direction in [0u8, 4, 8, 12] {
            let segment = rail_segment("straight-rail", direction).expect("a cardinal facing");
            assert_eq!(segment.length, 2.0, "direction {direction}");
        }
    }

    /// A diagonal covers two tiles on each axis, so it is longer than a
    /// cardinal piece rather than the same length turned.
    #[test]
    fn a_diagonal_rail_is_longer_than_a_cardinal_one() {
        let cardinal = rail_segment("straight-rail", 0).unwrap();
        let diagonal = rail_segment("straight-rail", 2).unwrap();
        assert!((diagonal.length - 8.0f32.sqrt()).abs() < 1e-5, "two tiles on each axis");
        assert!(diagonal.length > cardinal.length);
    }

    /// Facings eight apart are one segment seen from either end, so they have
    /// to draw identically. Getting this wrong would show as alternate pieces
    /// of a run pointing different ways.
    #[test]
    fn a_facing_and_its_opposite_draw_the_same_segment() {
        for name in ["straight-rail", "half-diagonal-rail"] {
            for direction in [0u8, 2, 4, 6] {
                assert_eq!(
                    rail_segment(name, direction),
                    rail_segment(name, direction + 8),
                    "{name} facing {direction} against {}",
                    direction + 8
                );
            }
        }
    }

    /// The 1:2 slope, which is what makes a half diagonal a different piece
    /// from a diagonal straight rather than a longer one.
    #[test]
    fn a_half_diagonal_runs_two_tiles_across_for_every_one_along() {
        for direction in [0u8, 2, 4, 6] {
            let segment = rail_segment("half-diagonal-rail", direction).unwrap();
            assert!((segment.length - 20.0f32.sqrt()).abs() < 1e-5, "direction {direction}");
            // 26.57 degrees from an axis, the angle of a 1:2 slope, whichever
            // axis it is measured against.
            let from_axis = segment.rotation.abs() % (std::f32::consts::PI / 2.0);
            let shallow = from_axis.min(std::f32::consts::PI / 2.0 - from_axis);
            assert!((shallow - 0.5f32.atan()).abs() < 1e-5, "direction {direction} sits at a 1:2 slope");
        }
    }

    /// Turning the piece a quarter turn turns its segment a quarter turn. This
    /// is the symmetry the two unmeasured half diagonal facings were derived
    /// from, so it is worth asserting rather than trusting.
    #[test]
    fn four_direction_steps_is_a_quarter_turn() {
        for name in ["straight-rail", "half-diagonal-rail"] {
            for direction in [0u8, 2] {
                let before = rail_segment(name, direction).unwrap();
                let after = rail_segment(name, direction + 4).unwrap();
                assert!((before.length - after.length).abs() < 1e-5, "{name}: a turn does not change the length");
                let turned = (after.rotation - before.rotation).abs() % std::f32::consts::PI;
                let quarter = (turned - std::f32::consts::FRAC_PI_2).abs();
                assert!(quarter < 1e-5, "{name} facing {direction}: turned by {turned} radians, wanted a quarter");
            }
        }
    }

    use save_timelapse::prototypes::{RailLink, RailSample};

    fn sample(name: &str, direction: u8, links: &[(&str, u8, f32, f32)]) -> RailSample {
        RailSample {
            name: name.to_string(),
            direction,
            links: links.iter().map(|&(n, d, x, y)| RailLink { name: n.to_string(), direction: d, x, y }).collect(),
        }
    }

    /// The whole point of the mod recording connectivity: a curve attached to
    /// a straight has one known joint, so its shape follows.
    ///
    /// A vertical straight at (0,3) relative to the curve reaches from (0,2)
    /// to (0,4), so it meets the curve at (0,2). A horizontal one at (-3,0)
    /// spans (-4,0) to (-2,0) and meets it at (-2,0). The curve therefore runs
    /// between those two points, which is nothing like the square it was.
    #[test]
    fn a_curve_takes_its_shape_from_the_rails_it_connects_to() {
        let solved = solve(&[sample("curved-rail-a", 0, &[("straight-rail", 0, 0.0, 3.0), ("straight-rail", 4, -3.0, 0.0)])]);

        let curve = solved.get(&("curved-rail-a".to_string(), 0)).expect("solved from its neighbours");
        let mut ends = curve.ends();
        ends.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((ends[0].0 - -2.0).abs() < 1e-4 && (ends[0].1 - 0.0).abs() < 1e-4, "got {ends:?}");
        assert!((ends[1].0 - 0.0).abs() < 1e-4 && (ends[1].1 - 2.0).abs() < 1e-4, "got {ends:?}");
        assert!((curve.length - 8.0f32.sqrt()).abs() < 1e-4, "the run between them");
    }

    /// A curve half attached to its twin rather than to a straight. The twin
    /// is unknown on the first round and known on the second, which is why the
    /// solve iterates instead of making a single pass.
    #[test]
    fn a_curve_attached_to_another_curve_resolves_on_a_later_round() {
        let solved = solve(&[
            sample("curved-rail-a", 0, &[("straight-rail", 0, 0.0, 3.0), ("straight-rail", 4, -3.0, 0.0)]),
            // Sits two tiles right and two up from the first curve, so the
            // first curve's (0,2) end is at (-2,0) in this one's frame.
            sample("curved-rail-b", 0, &[("curved-rail-a", 0, -2.0, 2.0), ("straight-rail", 4, 3.0, 0.0)]),
        ]);

        let twin = solved.get(&("curved-rail-b".to_string(), 0)).expect("resolved once its neighbour was");
        assert!(twin.length > 0.0);
    }

    /// A junction attaches more than two rails, and they meet the piece at the
    /// same two places. The run is the pair furthest apart, not whichever two
    /// happened to be recorded first.
    #[test]
    fn a_junction_does_not_shorten_the_piece_it_sits_on() {
        let solved = solve(&[sample(
            "curved-rail-a",
            0,
            &[
                ("straight-rail", 0, 0.0, 3.0),
                ("straight-rail", 4, -3.0, 0.0),
                // A third rail meeting it at a joint one of the others
                // already claimed.
                ("straight-rail", 0, 0.0, -1.0),
            ],
        )]);

        let curve = solved.get(&("curved-rail-a".to_string(), 0)).expect("still solved");
        assert!((curve.length - 8.0f32.sqrt()).abs() < 1e-4, "spans the outermost pair, got {}", curve.length);
    }

    /// The shape a real 2.0 corner has: two halves, each touching something
    /// known at one end and each other at the other, so neither ever gathers
    /// two joints. Before the pairing pass this returned nothing however many
    /// rounds it ran, and no amount of recorded connectivity would have
    /// helped.
    ///
    /// Measured off a real map: `curved-rail-a` facing 0 meets a vertical
    /// straight three tiles above it, which puts that joint at (0,2).
    #[test]
    fn a_corner_of_two_halves_resolves_even_though_neither_half_can_alone() {
        let solved = solve(&[
            sample("curved-rail-a", 0, &[("straight-rail", 0, 0.0, 3.0), ("curved-rail-b", 0, -2.0, -5.0)]),
            sample("curved-rail-b", 0, &[("curved-rail-a", 0, 2.0, 5.0), ("straight-rail", 4, -3.0, -2.0)]),
        ]);

        let first = solved.get(&("curved-rail-a".to_string(), 0)).expect("the half touching a straight");
        let second = solved.get(&("curved-rail-b".to_string(), 0)).expect("the half touching the first");
        assert!(first.length > 0.0 && second.length > 0.0);

        // They have to meet: the first's far end and the second's far end are
        // the same point once the second is put in the first's frame.
        let meeting = first.ends().iter().copied().find(|&(_, y)| y < 1.9).expect("the end away from the straight");
        let theirs: Vec<(f32, f32)> = second.ends().iter().map(|&(x, y)| (x - 2.0, y - 5.0)).collect();
        assert!(
            theirs.iter().any(|&t| (t.0 - meeting.0).abs() < 1e-3 && (t.1 - meeting.1).abs() < 1e-3),
            "the halves must share a joint: {meeting:?} against {theirs:?}"
        );
    }

    /// A piece with one anchor and no unknown partner is still left alone, so
    /// the pairing pass cannot invent geometry out of a dead end.
    #[test]
    fn one_anchor_and_nothing_to_pair_with_stays_unknown() {
        let solved = solve(&[sample("curved-rail-a", 0, &[("straight-rail", 0, 0.0, 3.0)])]);
        assert!(!solved.contains_key(&("curved-rail-a".to_string(), 0)));
    }

    /// Nothing to work from is left alone rather than guessed at.
    #[test]
    fn a_piece_with_one_joint_stays_unknown() {
        let solved = solve(&[sample("curved-rail-a", 0, &[("straight-rail", 0, 0.0, 3.0)])]);
        assert!(!solved.contains_key(&("curved-rail-a".to_string(), 0)));
    }

    /// A capture whose mod never described rails, which is every capture made
    /// before this existed.
    #[test]
    fn no_samples_solves_nothing_and_does_not_hang() {
        assert!(solve(&[]).is_empty());
    }

    /// A sample naming only prototypes this build has never heard of, which is
    /// a modded rail set. It resolves nothing and must not loop.
    #[test]
    fn samples_that_reference_nothing_known_resolve_nothing() {
        let solved = solve(&[sample("kr-curve", 0, &[("kr-curve", 4, 2.0, 0.0)])]);
        assert!(solved.is_empty());
    }

    /// 1.0's diagonals number their facings the other way round from 2.0's.
    /// Measured off a real map holding both: a legacy piece at facing 2 steps
    /// to the next by (+2,+2), where a 2.0 one steps by (+2,-2).
    ///
    /// Sharing one table drew every legacy diagonal at ninety degrees to the
    /// track it belongs to, which is what "the legacy rails look mangled"
    /// turned out to be.
    #[test]
    fn a_legacy_diagonal_runs_the_opposite_way_from_a_modern_one() {
        for (legacy_facing, modern_facing) in [(2u8, 6u8), (6, 2), (10, 14), (14, 10)] {
            let legacy = rail_segment("legacy-straight-rail", legacy_facing).unwrap();
            let modern = rail_segment("straight-rail", modern_facing).unwrap();
            assert!(
                (legacy.rotation - modern.rotation).abs() < 1e-5,
                "legacy facing {legacy_facing} should run like modern facing {modern_facing}"
            );
        }
    }

    /// The cardinals are the same in both, so only the diagonals swap.
    #[test]
    fn legacy_and_modern_cardinals_agree() {
        for facing in [0u8, 4, 8, 12] {
            assert_eq!(rail_segment("legacy-straight-rail", facing), rail_segment("straight-rail", facing), "facing {facing}");
        }
    }

    /// 1.0's corner is one piece for a whole quarter turn, and unlike a
    /// straight it does not sit at its own midpoint, so its offset carries
    /// where the run really is.
    #[test]
    fn a_legacy_curve_runs_between_the_ends_that_were_measured() {
        let curve = rail_segment("legacy-curved-rail", 0).expect("measured");
        let mut ends = curve.ends();
        ends.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((ends[0].0 - -2.0).abs() < 1e-4 && (ends[0].1 - -2.0).abs() < 1e-4, "got {ends:?}");
        assert!((ends[1].0 - 1.0).abs() < 1e-4 && (ends[1].1 - 4.0).abs() < 1e-4, "got {ends:?}");
        assert!(curve.offset != (0.0, 0.0), "a corner is not centred on the tile it is recorded at");
    }

    /// The symmetry the eight facings were checked against: a quarter turn of
    /// one facing is the next, and all eight are the same length. Getting this
    /// wrong would show as corners pointing into open ground.
    #[test]
    fn every_legacy_curve_facing_is_the_same_corner_turned() {
        let first = rail_segment("legacy-curved-rail", 0).unwrap();
        for facing in [2u8, 4, 6, 8, 10, 12, 14] {
            let turned = rail_segment("legacy-curved-rail", facing).unwrap();
            assert!((turned.length - first.length).abs() < 1e-4, "facing {facing} changed length");
        }
        // A quarter turn of facing 0's ends is facing 4's.
        let quarter: Vec<(f32, f32)> = first.ends().iter().map(|&(x, y)| (-y, x)).collect();
        let mut got = rail_segment("legacy-curved-rail", 4).unwrap().ends().to_vec();
        let mut want = quarter;
        let key = |p: &(f32, f32)| (p.0.to_bits(), p.1.to_bits());
        got.sort_by_key(key);
        want.sort_by_key(key);
        for (a, b) in got.iter().zip(&want) {
            assert!((a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4, "got {got:?}, wanted {want:?}");
        }
    }

    /// Curves are deliberately absent, and a caller has to keep working when
    /// this says nothing rather than assume every rail has an answer.
    #[test]
    fn a_curve_has_no_segment_yet() {
        assert_eq!(rail_segment("curved-rail-a", 0), None);
        assert_eq!(rail_segment("curved-rail-b", 4), None);
        assert_eq!(rail_segment("transport-belt", 0), None);
    }
}
