//! Which way a pipe joins its neighbours, and which of Factorio's pipe
//! pictures draws that.
//!
//! The same shape of problem as belt corners, and solved the same way. A pipe
//! has no direction of its own worth drawing: what it looks like depends
//! entirely on which of its four sides something joins onto. Factorio works
//! that out from the neighbours every time it redraws, and so does this, which
//! is what keeps it working on captures recorded long before this file existed
//! and costs nothing during play.
//!
//! Unlike belts, every piece is its own file rather than a row of one sheet:
//! `base/graphics/entity/pipe/` holds sixteen 128px pictures, one per
//! combination of the four sides. So a pipe's whole appearance is a four bit
//! mask, and drawing one is a table lookup.

use std::collections::HashMap;

use crate::render_frame::RenderEntity;

/// One bit per side, in Factorio's direction order.
pub const NORTH: u8 = 1;
pub const EAST: u8 = 2;
pub const SOUTH: u8 = 4;
pub const WEST: u8 = 8;

/// The picture for a pipe joined on exactly the sides in `mask`.
///
/// Every name lists the sides it connects, which is the reading the vertical
/// endings confirm: `pipe-ending-up` reaches the top of its frame exactly as
/// `pipe-straight-vertical` does, so "up" is the side it joins onto rather
/// than the side it is capped on.
///
/// The horizontal pair could not be settled the same way, because Factorio's
/// drop shadow falls to the lower right and inflates the right edge of every
/// sprite, so the bounding boxes disagree with the names until the shadow is
/// subtracted. This follows the vertical reading for consistency. If every
/// horizontal pipe end and corner comes out backwards in a real factory, that
/// is this decision, and flipping it is a matter of swapping the four names
/// that mention left and right.
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

/// The tile a one tile entity sits on. Pipes are always one tile.
fn tile_of(entity: &RenderEntity) -> (i32, i32) {
    (entity.x.floor() as i32, entity.y.floor() as i32)
}

/// Works out every pipe's connections and writes the mask into
/// `RenderEntity::shape`.
///
/// Only pipes joining pipes for now. A pipe also joins a `pipe-to-ground`
/// facing it and every fluid machine it touches, so a pipe running into a tank
/// or a pump draws as though it simply ended. That is the same trade the belt
/// corners started from: get the ordinary case right everywhere, on every
/// capture ever recorded, and add the rarer feeders once the common one is
/// known good.
pub fn infer_connections(entities: &mut [RenderEntity], is_pipe: &[bool]) {
    debug_assert_eq!(entities.len(), is_pipe.len());

    let mut pipes: HashMap<(i32, i32), ()> = HashMap::new();
    for (entity, &pipe) in entities.iter().zip(is_pipe) {
        if pipe {
            pipes.insert(tile_of(entity), ());
        }
    }
    if pipes.is_empty() {
        return;
    }

    for (entity, &pipe) in entities.iter_mut().zip(is_pipe) {
        if !pipe {
            continue;
        }
        let (x, y) = tile_of(entity);
        let mut mask = 0u8;
        for (bit, (dx, dy)) in [(NORTH, (0, -1)), (EAST, (1, 0)), (SOUTH, (0, 1)), (WEST, (-1, 0))] {
            if pipes.contains_key(&(x + dx, y + dy)) {
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

    fn masks(mut entities: Vec<RenderEntity>) -> Vec<u8> {
        let flags = vec![true; entities.len()];
        infer_connections(&mut entities, &flags);
        entities.iter().map(|e| e.shape).collect()
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

    /// Anything that is not a pipe is neither indexed nor written to.
    #[test]
    fn only_pipes_are_considered() {
        let mut entities = vec![pipe(0, 0), pipe(1, 0)];
        infer_connections(&mut entities, &[false, true]);
        assert_eq!(entities[1].shape, 0, "its only neighbour is not a pipe");
    }
}
