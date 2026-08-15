//! Which way a pipe joins its neighbours, and which of Factorio's pipe
//! pictures draws that.
//!
//! The same problem as belt corners, solved the same way: a pipe's appearance
//! depends entirely on which of its four sides something joins onto, worked
//! out from the neighbours rather than stored, which keeps it working on
//! captures recorded long before this file existed.
//!
//! Unlike belts, every piece is its own file: sixteen 128px pictures, one per
//! combination of sides. So a pipe's appearance is a four bit mask and drawing
//! one is a table lookup.

use std::collections::{HashMap, HashSet};

use crate::viewer::render_frame::RenderEntity;

/// One bit per side, in Factorio's direction order.
pub const NORTH: u8 = 1;
pub const EAST: u8 = 2;
pub const SOUTH: u8 = 4;
pub const WEST: u8 = 8;

/// Each side of a pipe: the bit it sets, the neighbouring tile, and the facing
/// a `pipe-to-ground` in that tile must have to open onto this pipe.
///
/// Those facings are Factorio's raw direction byte, which is 16-way, so the
/// cardinals are 0, 4, 8 and 12 clockwise from north. An underground pipe is
/// named and drawn for the side its above-ground opening faces (see
/// `sprites::pipe_to_ground_paths`), so the one that joins a pipe to its north
/// is the one facing back south.
const SIDES: [(u8, (i32, i32), u8); 4] = [(NORTH, (0, -1), 8), (EAST, (1, 0), 12), (SOUTH, (0, 1), 0), (WEST, (-1, 0), 4)];

/// The picture for a pipe joined on exactly the sides in `mask`.
///
/// Every name lists the sides it connects, which the vertical endings confirm:
/// `pipe-ending-up` reaches the top of its frame exactly as
/// `pipe-straight-vertical` does.
///
/// The horizontal pair could not be settled the same way, Factorio's drop
/// shadow inflating the right edge of every sprite, so this follows the
/// vertical reading. If every horizontal pipe end comes out backwards, that is
/// this decision.
pub fn piece_name(mask: u8) -> &'static str {
    match mask & 0b1111 {
        0 => "pipe-straight-vertical-single",

        NORTH => "pipe-ending-up",
        EAST => "pipe-ending-right",
        SOUTH => "pipe-ending-down",
        WEST => "pipe-ending-left",

        m if m == NORTH | SOUTH => "pipe-straight-vertical",
        m if m == EAST | WEST => "pipe-straight-horizontal",

        m if m == NORTH | EAST => "pipe-corner-up-right",
        m if m == NORTH | WEST => "pipe-corner-up-left",
        m if m == SOUTH | EAST => "pipe-corner-down-right",
        m if m == SOUTH | WEST => "pipe-corner-down-left",

        // A tee is named for its stem: the one side that leaves the straight
        // run. `t-up` joins north, east and west, so its stem points up.
        m if m == NORTH | EAST | WEST => "pipe-t-up",
        m if m == SOUTH | EAST | WEST => "pipe-t-down",
        m if m == NORTH | SOUTH | WEST => "pipe-t-left",
        m if m == NORTH | SOUTH | EAST => "pipe-t-right",

        _ => "pipe-cross",
    }
}

/// Every picture a pipe can need, in a fixed order so a mask indexes straight
/// into the textures loaded for it.
pub const PIECES: [&str; 16] = [
    "pipe-straight-vertical-single",
    "pipe-ending-up",
    "pipe-ending-right",
    "pipe-corner-up-right",
    "pipe-ending-down",
    "pipe-straight-vertical",
    "pipe-corner-down-right",
    "pipe-t-right",
    "pipe-ending-left",
    "pipe-corner-up-left",
    "pipe-straight-horizontal",
    "pipe-t-up",
    "pipe-corner-down-left",
    "pipe-t-left",
    "pipe-t-down",
    "pipe-cross",
];

/// Works out every pipe's connections and writes the mask into
/// `RenderEntity::shape`.
///
/// Pipes join pipes, and an underground pipe whose opening faces them: a run
/// that dives underground is most of the fluid on a real base, and without the
/// second half every one of those drew as a dead end.
///
/// Fluid machines are still missing, so a pipe running into a tank draws as
/// though it ended. That one cannot be inferred from position: where a machine
/// accepts fluid is a property of its prototype, which a capture does not
/// record, and adjacency alone would draw a stub at every pipe that happens to
/// run past an assembler.
pub fn infer_connections(entities: &mut [RenderEntity], is_pipe: &[bool], is_pipe_to_ground: &[bool]) {
    debug_assert_eq!(entities.len(), is_pipe.len());
    debug_assert_eq!(entities.len(), is_pipe_to_ground.len());

    let mut pipes: HashSet<(i32, i32)> = HashSet::new();
    // Facing per tile, which is the whole reason this one is a map: an
    // underground pipe joins on one side only.
    let mut undergrounds: HashMap<(i32, i32), u8> = HashMap::new();
    for ((entity, &pipe), &to_ground) in entities.iter().zip(is_pipe).zip(is_pipe_to_ground) {
        if pipe {
            pipes.insert(entity.tile());
        } else if to_ground {
            undergrounds.insert(entity.tile(), entity.d);
        }
    }
    if pipes.is_empty() {
        return;
    }

    for (entity, &pipe) in entities.iter_mut().zip(is_pipe) {
        if !pipe {
            continue;
        }
        let (x, y) = entity.tile();
        let mut mask = 0u8;
        for (bit, (dx, dy), opening) in SIDES {
            let neighbour = (x + dx, y + dy);
            if pipes.contains(&neighbour) || undergrounds.get(&neighbour) == Some(&opening) {
                mask |= bit;
            }
        }
        entity.shape = mask;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe(x: i32, y: i32) -> RenderEntity {
        RenderEntity { x: x as f32 + 0.5, y: y as f32 + 0.5, w: 1, h: 1, d: 0, shape: 0 }
    }

    /// An underground pipe at `(x, y)` whose opening faces `facing`, as
    /// Factorio's raw direction byte.
    fn to_ground(x: i32, y: i32, facing: u8) -> RenderEntity {
        RenderEntity { d: facing, ..pipe(x, y) }
    }

    fn masks(mut entities: Vec<RenderEntity>) -> Vec<u8> {
        let flags = vec![true; entities.len()];
        let none = vec![false; entities.len()];
        infer_connections(&mut entities, &flags, &none);
        entities.iter().map(|e| e.shape).collect()
    }

    /// The mask on the pipe at index 0, with every later entity an underground
    /// pipe rather than a pipe.
    fn mask_beside_undergrounds(mut entities: Vec<RenderEntity>) -> u8 {
        let pipes: Vec<bool> = (0..entities.len()).map(|i| i == 0).collect();
        let to_grounds: Vec<bool> = (0..entities.len()).map(|i| i != 0).collect();
        infer_connections(&mut entities, &pipes, &to_grounds);
        entities[0].shape
    }

    /// The table and the ordered list have to agree, or a mask would pick one
    /// picture to load and a different one to draw.
    #[test]
    fn the_piece_order_matches_the_lookup() {
        for mask in 0u8..16 {
            assert_eq!(PIECES[mask as usize], piece_name(mask), "mask {mask:04b}");
        }
    }

    /// All sixteen are distinct pictures, so nothing is silently unreachable.
    #[test]
    fn every_combination_has_its_own_picture() {
        let mut seen: Vec<&str> = Vec::new();
        for mask in 0u8..16 {
            let name = piece_name(mask);
            assert!(!seen.contains(&name), "{name} used twice");
            seen.push(name);
        }
    }

    #[test]
    fn a_lone_pipe_connects_to_nothing() {
        assert_eq!(masks(vec![pipe(0, 0)]), vec![0]);
        assert_eq!(piece_name(0), "pipe-straight-vertical-single");
    }

    /// A run east to west: the ends cap, the middle runs straight through.
    #[test]
    fn a_straight_run_caps_at_both_ends() {
        let found = masks(vec![pipe(0, 0), pipe(1, 0), pipe(2, 0)]);
        assert_eq!(piece_name(found[0]), "pipe-ending-right", "joined only on its east side");
        assert_eq!(piece_name(found[1]), "pipe-straight-horizontal");
        assert_eq!(piece_name(found[2]), "pipe-ending-left", "joined only on its west side");
    }

    /// An elbow: joined north and east, so the corner that names both.
    #[test]
    fn a_bend_picks_the_corner_naming_both_sides() {
        let found = masks(vec![pipe(0, 0), pipe(0, -1), pipe(1, 0)]);
        assert_eq!(found[0], NORTH | EAST);
        assert_eq!(piece_name(found[0]), "pipe-corner-up-right");
    }

    #[test]
    fn a_junction_of_three_is_a_tee_named_for_its_stem() {
        // Joined north, east and west: the stem points up.
        let found = masks(vec![pipe(0, 0), pipe(0, -1), pipe(1, 0), pipe(-1, 0)]);
        assert_eq!(piece_name(found[0]), "pipe-t-up");
    }

    #[test]
    fn a_junction_of_four_is_a_cross() {
        let found = masks(vec![pipe(0, 0), pipe(0, -1), pipe(0, 1), pipe(1, 0), pipe(-1, 0)]);
        assert_eq!(found[0], 0b1111);
        assert_eq!(piece_name(found[0]), "pipe-cross");
    }

    /// Diagonal neighbours are not connections, which is the mistake a naive
    /// "is there a pipe nearby" check makes, and pipes run in dense blocks.
    #[test]
    fn diagonal_neighbours_do_not_connect() {
        let found = masks(vec![pipe(0, 0), pipe(1, 1), pipe(-1, -1)]);
        assert_eq!(found[0], 0);
    }

    /// Anything that is neither a pipe nor an underground pipe is neither
    /// indexed nor written to.
    #[test]
    fn only_pipes_are_considered() {
        let mut entities = vec![pipe(0, 0), pipe(1, 0)];
        infer_connections(&mut entities, &[false, true], &[false, false]);
        assert_eq!(entities[1].shape, 0, "its only neighbour is not a pipe");
    }

    /// The bug this exists for: a run diving underground is most of the fluid
    /// on a real base, and the pipe meeting it drew as a dead end.
    ///
    /// One per side, each underground pipe facing back at the pipe between
    /// them, which is the arrangement that actually carries fluid.
    #[test]
    fn an_underground_pipe_opening_onto_a_pipe_joins_it() {
        assert_eq!(mask_beside_undergrounds(vec![pipe(0, 0), to_ground(0, -1, 8)]), NORTH);
        assert_eq!(mask_beside_undergrounds(vec![pipe(0, 0), to_ground(1, 0, 12)]), EAST);
        assert_eq!(mask_beside_undergrounds(vec![pipe(0, 0), to_ground(0, 1, 0)]), SOUTH);
        assert_eq!(mask_beside_undergrounds(vec![pipe(0, 0), to_ground(-1, 0, 4)]), WEST);
    }

    /// An underground pipe joins on exactly one side, so the three facings that
    /// are not pointing at the pipe must leave it capped. Getting this wrong in
    /// the forgiving direction would join every pipe that runs past one.
    #[test]
    fn an_underground_pipe_facing_any_other_way_does_not() {
        for facing in [0, 4, 12] {
            assert_eq!(
                mask_beside_undergrounds(vec![pipe(0, 0), to_ground(0, -1, facing)]),
                0,
                "an underground pipe to the north facing {facing} does not open onto the pipe"
            );
        }
    }

    /// A pipe elbowing into an underground run: one real neighbour and one
    /// underground, which have to reach the same mask to draw as a corner.
    #[test]
    fn a_pipe_corners_between_a_pipe_and_an_underground_one() {
        let mut entities = vec![pipe(0, 0), pipe(0, -1), to_ground(1, 0, 12)];
        infer_connections(&mut entities, &[true, true, false], &[false, false, true]);
        assert_eq!(entities[0].shape, NORTH | EAST);
        assert_eq!(piece_name(entities[0].shape), "pipe-corner-up-right");
    }

    /// Underground pipes are read but never written to: their own picture comes
    /// from their facing, not from a mask.
    #[test]
    fn an_underground_pipe_gets_no_mask_of_its_own() {
        let mut entities = vec![pipe(0, 0), to_ground(1, 0, 12)];
        infer_connections(&mut entities, &[true, false], &[false, true]);
        assert_eq!(entities[1].shape, 0);
    }

    /// A pair facing each other with nothing between them is the ordinary way
    /// an underground run ends, and must not crash or write anything.
    #[test]
    fn undergrounds_alone_are_left_untouched() {
        let mut entities = vec![to_ground(0, 0, 4), to_ground(3, 0, 12)];
        infer_connections(&mut entities, &[false, false], &[true, true]);
        assert_eq!(entities.iter().map(|e| e.shape).collect::<Vec<_>>(), vec![0, 0]);
    }

    /// The synthetic cases prove the rule; this proves the arrangement is one
    /// real factories actually build, so a reading of Factorio's facings that
    /// is backwards shows up as nothing joining rather than as a passing suite.
    ///
    /// Counted against the same frame with the underground pipes withheld,
    /// which is exactly what this file did before.
    #[test]
    fn a_real_capture_has_pipes_meeting_underground_ones() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames");
        let frames = crate::viewer::loading::load_sequence(std::path::Path::new(dir)).unwrap();
        let frame = frames.into_iter().last().expect("the fixture has frames");

        let mut registry = crate::viewer::registry::TypeRegistry::new();
        let ids: Vec<_> = frame.entities.iter().map(|e| registry.intern(&e.n)).collect();
        let pipe_flags: Vec<bool> = ids.iter().map(|&id| registry.is_pipe(id)).collect();
        let to_ground_flags: Vec<bool> = ids.iter().map(|&id| registry.is_pipe_to_ground(id)).collect();
        assert!(pipe_flags.iter().any(|&p| p), "the fixture must contain pipes for this to prove anything");
        assert!(to_ground_flags.iter().any(|&p| p), "and underground ones");

        let mut entities: Vec<RenderEntity> = frame
            .entities
            .iter()
            .map(|e| RenderEntity { x: e.x, y: e.y, w: e.w as u8, h: e.h as u8, d: e.d, shape: 0 })
            .collect();

        let no_undergrounds = vec![false; entities.len()];
        infer_connections(&mut entities, &pipe_flags, &no_undergrounds);
        let before: usize = entities.iter().map(|e| e.shape.count_ones() as usize).sum();
        infer_connections(&mut entities, &pipe_flags, &to_ground_flags);
        let after: usize = entities.iter().map(|e| e.shape.count_ones() as usize).sum();

        assert!(after > before, "no pipe in a real capture met an underground one: {before} connections either way");
    }
}
