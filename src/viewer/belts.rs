//! Which way a belt bends, and which frame of Factorio's own belt sheet draws
//! it.
//!
//! Factorio draws corners from a separate picture rather than a rotated
//! straight one: `transport-belt.png` is 20 rows of square frames, four
//! straight facings, eight corners and eight end caps, each animated across
//! its row. Every orientation is drawn separately, which is why nothing here
//! mirrors or rotates anything. Picking the row is the whole job.
//!
//! Nothing in a belt's record says whether it is curved, and Factorio does not
//! store that either: it derives the shape from the neighbours every time, and
//! so does `infer_shapes`. That keeps this working on captures recorded long
//! before this file existed.

use std::collections::HashMap;

use crate::viewer::render_frame::RenderEntity;

/// Factorio's cardinal direction bytes. The raw field is 16-way (22.5 degree
/// steps) but a belt only ever sits on one of these four.
const NORTH: u8 = 0;
const EAST: u8 = 4;
const SOUTH: u8 = 8;
const WEST: u8 = 12;

/// Which way a belt bends, as Factorio's own `LuaEntity.belt_shape` reports
/// it: relative to the direction of travel, so a belt carrying items east that
/// turns to carry them north has turned left.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BeltShape {
    #[default]
    Straight,
    Left,
    Right,
}

impl BeltShape {
    /// Packed into `RenderEntity::shape`, which is a byte rather than this
    /// enum so the struct stays in the padding it already had.
    pub fn from_byte(byte: u8) -> BeltShape {
        match byte {
            1 => BeltShape::Left,
            2 => BeltShape::Right,
            _ => BeltShape::Straight,
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            BeltShape::Straight => 0,
            BeltShape::Left => 1,
            BeltShape::Right => 2,
        }
    }
}

/// Rows in the belt sheet, zero based, from Factorio's own
/// `basic_belt_animation_set`, which lists them one based.
///
/// A corner is named for the two sides it joins, not for the headings of the
/// items crossing it: `EAST_TO_NORTH` takes items in through its east edge and
/// out through its north. Those readings are opposites, an item entering
/// through the east edge being one travelling west.
const EAST_ROW: usize = 0;
const WEST_ROW: usize = 1;
const NORTH_ROW: usize = 2;
const SOUTH_ROW: usize = 3;
const EAST_TO_NORTH: usize = 4;
const NORTH_TO_EAST: usize = 5;
const WEST_TO_NORTH: usize = 6;
const NORTH_TO_WEST: usize = 7;
const SOUTH_TO_EAST: usize = 8;
const EAST_TO_SOUTH: usize = 9;
const SOUTH_TO_WEST: usize = 10;
const WEST_TO_SOUTH: usize = 11;

/// How many rows the sheet has, including the end caps this does not use.
/// Layout is derived from the file's own dimensions, the four tiers having
/// different animation lengths while sharing these 20 rows.
pub const SHEET_ROWS: usize = 20;

/// The sheet row that draws a belt facing `direction` with shape `shape`.
/// `None` for a non-cardinal direction, which a belt should never be; the
/// caller falls back rather than guessing a row.
pub fn sheet_row(direction: u8, shape: BeltShape) -> Option<usize> {
    let row = match (direction, shape) {
        (NORTH, BeltShape::Straight) => NORTH_ROW,
        (EAST, BeltShape::Straight) => EAST_ROW,
        (SOUTH, BeltShape::Straight) => SOUTH_ROW,
        (WEST, BeltShape::Straight) => WEST_ROW,

        // A left turn is fed from the belt's left side, so the frame wanted is
        // the one named for that side. Facing north, left is west, which is
        // `WEST_TO_NORTH`. Naming it for the arriving heading gives exactly
        // the opposite frame.
        (NORTH, BeltShape::Left) => WEST_TO_NORTH,
        (EAST, BeltShape::Left) => NORTH_TO_EAST,
        (SOUTH, BeltShape::Left) => EAST_TO_SOUTH,
        (WEST, BeltShape::Left) => SOUTH_TO_WEST,

        (NORTH, BeltShape::Right) => EAST_TO_NORTH,
        (EAST, BeltShape::Right) => SOUTH_TO_EAST,
        (SOUTH, BeltShape::Right) => WEST_TO_SOUTH,
        (WEST, BeltShape::Right) => NORTH_TO_WEST,

        _ => return None,
    };
    Some(row)
}

/// One tile step in `direction`, in tile coordinates with y increasing south,
/// which is the convention Factorio positions already use.
fn step(direction: u8) -> (i32, i32) {
    match direction {
        NORTH => (0, -1),
        EAST => (1, 0),
        SOUTH => (0, 1),
        WEST => (-1, 0),
        _ => (0, 0),
    }
}

fn clockwise(direction: u8) -> u8 {
    (direction + 4) % 16
}

fn anticlockwise(direction: u8) -> u8 {
    (direction + 12) % 16
}

/// The two tiles a splitter covers, from its facing rather than its reported
/// footprint: a splitter is always two across its facing and one deep, which
/// needs no field to confirm. Reading width and height instead depends on
/// those arriving the right way round for a rotated entity, and swapped they
/// leave exactly one of its two outputs feeding.
fn splitter_tiles(entity: &RenderEntity) -> Vec<(i32, i32)> {
    let (across_x, across_y) = if step(entity.d).0 == 0 { (2, 1) } else { (1, 2) };
    let x0 = (entity.x - across_x as f32 / 2.0).round() as i32;
    let y0 = (entity.y - across_y as f32 / 2.0).round() as i32;
    (0..across_x).flat_map(|ox| (0..across_y).map(move |oy| (ox, oy))).map(|(ox, oy)| (x0 + ox, y0 + oy)).collect()
}

/// What kind of thing moves items along, for working out belt corners. Only
/// belts get a shape written; the rest are here because they can bend one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Carrier {
    Belt,
    Splitter,
    Underground,
}

/// Works out every belt's curve from its neighbours and writes it into
/// `RenderEntity::shape`.
///
/// Factorio's rule: a belt fed from directly behind runs straight, one fed
/// from exactly one side curves towards that side, and anything else (fed from
/// both, or from nothing) draws straight.
///
/// `is_belt` marks which entries are belts by index, a `RenderEntity` not
/// carrying its own type.
///
/// Anything that puts items on the ground counts as a feeder, not just belts:
/// a splitter's output and an underground exit bend a belt exactly as another
/// belt would. Loaders are still missing. This is also why underground ends
/// are resolved first: an exit only counts as a feeder once known to be one.
pub fn infer_shapes(entities: &mut [RenderEntity], kinds: &[Option<Carrier>]) {
    debug_assert_eq!(entities.len(), kinds.len());

    // Every tile that pushes items out, and which way. Built over carriers
    // alone: a megabase is mostly not belts, so keeping the rest out saves the
    // map most of its size for one pass.
    let mut belts: HashMap<(i32, i32), u8> = HashMap::new();
    for (entity, kind) in entities.iter().zip(kinds) {
        match kind {
            Some(Carrier::Belt) => {
                belts.insert(entity.tile(), entity.d);
            }
            // A splitter is two tiles across and one deep, so every tile it
            // covers sits on its output edge and feeds the tile beyond.
            Some(Carrier::Splitter) => {
                for tile in splitter_tiles(entity) {
                    belts.insert(tile, entity.d);
                }
            }
            // Only the far end of a crossing puts anything back on the
            // ground. The entrance swallows items, so it feeds nothing and a
            // belt beside it must not bend towards it.
            Some(Carrier::Underground) if UndergroundEnd::from_byte(entity.shape) == UndergroundEnd::Exit => {
                belts.insert(entity.tile(), entity.d);
            }
            _ => {}
        }
    }
    if belts.is_empty() {
        return;
    }

    // Whether the belt one step in `from` of `tile` points back at it.
    let feeds = |tile: (i32, i32), from: u8, facing: u8| {
        let (dx, dy) = step(from);
        belts.get(&(tile.0 + dx, tile.1 + dy)) == Some(&facing)
    };

    for (entity, kind) in entities.iter_mut().zip(kinds) {
        if *kind != Some(Carrier::Belt) {
            continue;
        }
        let d = entity.d;
        if step(d) == (0, 0) {
            continue;
        }
        let tile = entity.tile();

        // Behind is opposite the facing, and a belt there feeds this one only
        // if it faces the same way.
        let behind = feeds(tile, anticlockwise(anticlockwise(d)), d);
        if behind {
            entity.shape = BeltShape::Straight.to_byte();
            continue;
        }
        // A belt on this one's left, facing across into it, arrives heading
        // clockwise of this belt's facing, which is a left turn.
        let from_left = feeds(tile, anticlockwise(d), clockwise(d));
        let from_right = feeds(tile, clockwise(d), anticlockwise(d));
        entity.shape = match (from_left, from_right) {
            (true, false) => BeltShape::Left,
            (false, true) => BeltShape::Right,
            _ => BeltShape::Straight,
        }
        .to_byte();
    }
}

/// Which end of an underground belt crossing this is.
///
/// Both ends carry the same direction, both moving items the same way, so
/// direction alone cannot tell them apart. Factorio draws them as two separate
/// structures, `direction_in` and `direction_out`, facing opposite ways.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum UndergroundEnd {
    /// Where items go down. Factorio's `direction_in`.
    #[default]
    Entrance,
    /// Where they come back up. Factorio's `direction_out`.
    Exit,
}

impl UndergroundEnd {
    pub fn from_byte(byte: u8) -> UndergroundEnd {
        match byte {
            1 => UndergroundEnd::Exit,
            _ => UndergroundEnd::Entrance,
        }
    }

    /// Row in the structure sheet, whose four rows are `direction_out`,
    /// `direction_in`, and two side-loading variants this does not use. Read
    /// off the prototype's `y` offsets, out coming before in being the
    /// opposite of what the names suggest.
    pub fn sheet_row(self) -> usize {
        match self {
            UndergroundEnd::Exit => 0,
            UndergroundEnd::Entrance => 1,
        }
    }
}

/// Sorts each line of underground belts into flow order and pairs them, so a
/// crossing gets one entrance and one exit.
///
/// Pairing rather than judging each alone, since an entrance and an exit
/// facing the same way are the same record. Walking a line in flow order two
/// at a time is how the game pairs them, and handles several crossings sharing
/// one line.
///
/// `kinds` gives the tier and reach per underground belt, `None` otherwise.
pub fn infer_underground_ends(entities: &mut [RenderEntity], kinds: &[Option<(u16, i32)>]) {
    debug_assert_eq!(entities.len(), kinds.len());

    // Keyed by tier, facing, and the line the crossing runs along, so only
    // belts that could possibly pair with each other are ever compared.
    let mut lines: HashMap<(u16, u8, i32), Vec<usize>> = HashMap::new();
    for (index, kind) in kinds.iter().enumerate() {
        let Some((tier, _)) = *kind else { continue };
        let d = entities[index].d;
        if step(d) == (0, 0) {
            continue;
        }
        let (x, y) = entities[index].tile();
        // A crossing running north/south stays on one column, east/west on one
        // row, so the other coordinate identifies the line.
        let across = if step(d).0 == 0 { x } else { y };
        lines.entry((tier, d, across)).or_default().push(index);
    }

    for ((_, d, _), mut along) in lines {
        let (dx, dy) = step(d);
        // Position along the flow, so sorting ascending puts them in the order
        // items reach them whichever way the crossing points.
        let travelled = |entities: &[RenderEntity], i: usize| {
            let (x, y) = entities[i].tile();
            x * dx + y * dy
        };
        along.sort_by_key(|&i| travelled(entities, i));

        let mut at = 0;
        while at < along.len() {
            let first = along[at];
            let partner = along.get(at + 1).copied();
            let paired = partner.is_some_and(|next| {
                let gap = travelled(entities, next) - travelled(entities, first);
                gap <= kinds[first].map_or(0, |(_, reach)| reach)
            });
            entities[first].shape = UndergroundEnd::Entrance as u8;
            match paired {
                true => {
                    entities[partner.expect("paired implies a partner")].shape = UndergroundEnd::Exit as u8;
                    at += 2;
                }
                // Too far to connect, or nothing after it. Factorio draws a
                // lone underground belt as an entrance, so this matches.
                false => at += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape byte has to be free, or this costs a byte on every entity of a
    /// megabase to describe four prototypes. `RenderEntity`'s four byte
    /// alignment already left one spare.
    #[test]
    fn the_shape_byte_costs_no_memory() {
        assert_eq!(std::mem::size_of::<RenderEntity>(), 12);
    }

    /// Every cardinal facing and every shape resolves to a distinct row, and
    /// the twelve rows used are exactly the four straights plus eight corners.
    #[test]
    fn every_facing_and_shape_maps_to_its_own_row() {
        let mut seen = Vec::new();
        for direction in [NORTH, EAST, SOUTH, WEST] {
            for shape in [BeltShape::Straight, BeltShape::Left, BeltShape::Right] {
                let row = sheet_row(direction, shape).expect("cardinal directions all resolve");
                assert!(row < SHEET_ROWS, "row {row} is off the sheet");
                assert!(!seen.contains(&row), "row {row} used twice");
                seen.push(row);
            }
        }
        assert_eq!(seen.len(), 12);
    }

    #[test]
    fn a_diagonal_direction_has_no_row() {
        assert_eq!(sheet_row(2, BeltShape::Straight), None);
        assert_eq!(sheet_row(7, BeltShape::Left), None);
    }

    fn belt(x: i32, y: i32, d: u8) -> RenderEntity {
        RenderEntity { x: x as f32 + 0.5, y: y as f32 + 0.5, w: 1, h: 1, d, shape: 0 }
    }

    fn shapes(mut entities: Vec<RenderEntity>) -> Vec<BeltShape> {
        let flags = vec![Some(Carrier::Belt); entities.len()];
        infer_shapes(&mut entities, &flags);
        entities.iter().map(|e| BeltShape::from_byte(e.shape)).collect()
    }

    /// A straight run: each belt is fed from directly behind.
    #[test]
    fn a_straight_run_stays_straight() {
        let found = shapes(vec![belt(0, 0, EAST), belt(1, 0, EAST), belt(2, 0, EAST)]);
        assert_eq!(found, vec![BeltShape::Straight; 3]);
    }

    /// Items travel east along y=0 and turn to travel north. The corner belt
    /// faces north and is fed through its west edge, which is a left turn, and
    /// the frame for that is the one named for the edge items come in by.
    #[test]
    fn a_corner_fed_through_its_west_edge_curves_left() {
        let found = shapes(vec![belt(0, 0, EAST), belt(1, 0, NORTH), belt(1, -1, NORTH)]);
        assert_eq!(found[1], BeltShape::Left, "the corner");
        assert_eq!(found[2], BeltShape::Straight, "fed from behind by the corner");
        assert_eq!(sheet_row(NORTH, found[1]), Some(WEST_TO_NORTH));
    }

    /// The mirror case: fed through the east edge is a right turn.
    #[test]
    fn a_corner_fed_through_its_east_edge_curves_right() {
        let found = shapes(vec![belt(2, 0, WEST), belt(1, 0, NORTH)]);
        assert_eq!(found[1], BeltShape::Right);
        assert_eq!(sheet_row(NORTH, found[1]), Some(EAST_TO_NORTH));
    }

    /// Every corner drew with its incoming edge flipped, from reading
    /// `east_to_north` as "arrives heading east" when it means "enters through
    /// the east edge".
    #[test]
    fn corners_take_items_in_by_the_edge_they_are_fed_from() {
        // Fed from above, out to the left: in through the north edge.
        let found = shapes(vec![belt(1, -1, SOUTH), belt(1, 0, WEST)]);
        assert_eq!(sheet_row(WEST, found[1]), Some(NORTH_TO_WEST), "top to left");

        // Fed from above, out to the right: in through the north edge.
        let found = shapes(vec![belt(1, -1, SOUTH), belt(1, 0, EAST)]);
        assert_eq!(sheet_row(EAST, found[1]), Some(NORTH_TO_EAST), "top to right");

        // And the two that were being drawn instead, now reached only by a
        // corner genuinely fed from below.
        let found = shapes(vec![belt(1, 1, NORTH), belt(1, 0, WEST)]);
        assert_eq!(sheet_row(WEST, found[1]), Some(SOUTH_TO_WEST), "bottom to left");

        let found = shapes(vec![belt(1, 1, NORTH), belt(1, 0, EAST)]);
        assert_eq!(sheet_row(EAST, found[1]), Some(SOUTH_TO_EAST), "bottom to right");
    }

    /// Fed from both sides with nothing behind is a merge, which Factorio
    /// draws straight rather than picking one of the two corners.
    #[test]
    fn a_belt_fed_from_both_sides_draws_straight() {
        let found = shapes(vec![belt(0, 0, EAST), belt(2, 0, WEST), belt(1, 0, NORTH)]);
        assert_eq!(found[2], BeltShape::Straight);
    }

    /// A belt behind wins over a belt to the side, so a side feed joining a
    /// straight run does not bend it.
    #[test]
    fn a_feed_from_behind_beats_a_feed_from_the_side() {
        let found = shapes(vec![belt(0, 0, EAST), belt(1, 0, EAST), belt(1, 1, NORTH)]);
        assert_eq!(found[1], BeltShape::Straight, "fed from behind and from the south side");
    }

    /// A belt beside another that merely runs alongside it feeds nothing, so
    /// neither bends. This is the case a naive "is there a belt next to me"
    /// check gets wrong, and parallel belt lanes are everywhere.
    #[test]
    fn belts_running_alongside_each_other_do_not_bend() {
        let found = shapes(vec![belt(0, 0, NORTH), belt(1, 0, NORTH), belt(0, 1, NORTH), belt(1, 1, NORTH)]);
        assert_eq!(found, vec![BeltShape::Straight; 4]);
    }

    /// A belt leaving a splitter or an underground exit bends like one leaving
    /// another belt. The gap was one-sided: a belt running into a splitter
    /// curved, the one coming out did not.
    #[test]
    fn a_belt_fed_by_a_splitter_or_an_underground_exit_curves() {
        // A splitter facing north, centred on the boundary between tiles 0 and
        // 1, feeds both tiles above it. The belt on one turns west, so it is
        // fed through its east edge; the splitter is south of it, and facing
        // west the left side is south.
        let splitter = RenderEntity { x: 1.0, y: 0.5, w: 2, h: 1, d: NORTH, shape: 0 };
        let mut entities = vec![splitter, belt(1, -1, WEST)];
        infer_shapes(&mut entities, &[Some(Carrier::Splitter), Some(Carrier::Belt)]);
        let shape = BeltShape::from_byte(entities[1].shape);
        assert_eq!(shape, BeltShape::Left, "leaving a splitter");
        assert_eq!(sheet_row(WEST, shape), Some(SOUTH_TO_WEST));

        // An underground exit facing east feeds the tile in front of it.
        let mut exit = belt(0, 0, EAST);
        exit.shape = UndergroundEnd::Exit as u8;
        let mut entities = vec![exit, belt(1, 0, NORTH)];
        infer_shapes(&mut entities, &[Some(Carrier::Underground), Some(Carrier::Belt)]);
        assert_eq!(BeltShape::from_byte(entities[1].shape), BeltShape::Left, "leaving an underground exit");

        // The entrance swallows items, so a belt beside one must not bend
        // towards it. This is what stops both ends of a crossing acting alike.
        let mut entrance = belt(0, 0, EAST);
        entrance.shape = UndergroundEnd::Entrance as u8;
        let mut entities = vec![entrance, belt(1, 0, NORTH)];
        infer_shapes(&mut entities, &[Some(Carrier::Underground), Some(Carrier::Belt)]);
        assert_eq!(BeltShape::from_byte(entities[1].shape), BeltShape::Straight, "beside an entrance");
    }

    /// A splitter facing east covers two tiles stacked north to south, and the
    /// belt off its lower output turning south is fed through its west edge.
    #[test]
    fn a_belt_off_a_sideways_splitters_lower_output_curves() {
        // 1 wide by 2 tall, so its centre sits on a tile boundary in y.
        let splitter = RenderEntity { x: 10.5, y: 20.0, w: 1, h: 2, d: EAST, shape: 0 };
        let mut entities = vec![splitter, belt(11, 19, EAST), belt(11, 20, SOUTH)];
        infer_shapes(&mut entities, &[Some(Carrier::Splitter), Some(Carrier::Belt), Some(Carrier::Belt)]);
        let lower = BeltShape::from_byte(entities[2].shape);
        assert_eq!(lower, BeltShape::Right, "fed from the west by the splitter");
        assert_eq!(sheet_row(SOUTH, lower), Some(WEST_TO_SOUTH));
    }

    /// The full arrangement: a sideways splitter fed by two belts curving in
    /// and feeding two curving out, all four bending away from the line.
    #[test]
    fn a_splitter_with_two_curves_in_and_two_out_bends_all_four() {
        let splitter = RenderEntity { x: 10.5, y: 20.0, w: 1, h: 2, d: EAST, shape: 0 };
        let mut entities = vec![
            splitter,
            // Feeding in: each arrives from beyond the splitter's span and
            // turns east onto it.
            belt(9, 18, SOUTH),
            belt(9, 19, EAST),
            belt(9, 21, NORTH),
            belt(9, 20, EAST),
            // Coming out: fanning apart.
            belt(11, 19, NORTH),
            belt(11, 20, SOUTH),
        ];
        let kinds = vec![
            Some(Carrier::Splitter),
            Some(Carrier::Belt),
            Some(Carrier::Belt),
            Some(Carrier::Belt),
            Some(Carrier::Belt),
            Some(Carrier::Belt),
            Some(Carrier::Belt),
        ];
        infer_shapes(&mut entities, &kinds);
        let shape = |i: usize| BeltShape::from_byte(entities[i].shape);

        assert_eq!(shape(2), BeltShape::Left, "upper input turning east");
        assert_eq!(shape(4), BeltShape::Right, "lower input turning east");
        assert_eq!(shape(5), BeltShape::Left, "upper output turning north");
        assert_eq!(shape(6), BeltShape::Right, "lower output turning south");
    }

    /// Both output tiles of an upward splitter feed, not just one. Its centre
    /// sits on the boundary between them, and getting that tile wrong by one
    /// leaves exactly one output working.
    #[test]
    fn both_outputs_of_an_upward_splitter_feed() {
        // Covers tiles (9, 20) and (10, 20), so it feeds (9, 19) and (10, 19).
        let splitter = RenderEntity { x: 10.0, y: 20.5, w: 2, h: 1, d: NORTH, shape: 0 };
        let mut entities = vec![splitter, belt(9, 19, WEST), belt(10, 19, EAST)];
        let kinds = vec![Some(Carrier::Splitter), Some(Carrier::Belt), Some(Carrier::Belt)];
        infer_shapes(&mut entities, &kinds);

        assert_eq!(BeltShape::from_byte(entities[1].shape), BeltShape::Left, "left output");
        assert_eq!(BeltShape::from_byte(entities[2].shape), BeltShape::Right, "right output");
    }

    fn ends(entities: Vec<RenderEntity>, reach: i32) -> Vec<UndergroundEnd> {
        let mut entities = entities;
        let kinds: Vec<Option<(u16, i32)>> = vec![Some((0, reach)); entities.len()];
        infer_underground_ends(&mut entities, &kinds);
        entities.iter().map(|e| UndergroundEnd::from_byte(e.shape)).collect()
    }

    /// The pair reads in flow order: items go down at the first one they reach
    /// and come back up at the second.
    #[test]
    fn a_crossing_is_an_entrance_then_an_exit() {
        assert_eq!(ends(vec![belt(0, 0, EAST), belt(4, 0, EAST)], 5), vec![UndergroundEnd::Entrance, UndergroundEnd::Exit]);
    }

    /// Flow order, not coordinate order. A crossing pointing west starts at
    /// the higher x, so sorting by x alone would label it backwards.
    #[test]
    fn a_crossing_pointing_west_starts_at_its_higher_coordinate() {
        let found = ends(vec![belt(0, 0, WEST), belt(4, 0, WEST)], 5);
        assert_eq!(found, vec![UndergroundEnd::Exit, UndergroundEnd::Entrance]);
    }

    /// Two separate crossings sharing one line pair up individually rather
    /// than the outermost two pairing with each other.
    #[test]
    fn separate_crossings_on_one_line_pair_up_individually() {
        let found = ends(vec![belt(0, 0, EAST), belt(3, 0, EAST), belt(6, 0, EAST), belt(9, 0, EAST)], 5);
        assert_eq!(found, vec![UndergroundEnd::Entrance, UndergroundEnd::Exit, UndergroundEnd::Entrance, UndergroundEnd::Exit]);
    }

    /// Further apart than the tier can reach is not a pair, so neither is an
    /// exit. Factorio draws a lone underground belt as an entrance.
    #[test]
    fn belts_too_far_apart_to_connect_are_not_a_pair() {
        assert_eq!(ends(vec![belt(0, 0, EAST), belt(9, 0, EAST)], 5), vec![UndergroundEnd::Entrance; 2]);
    }

    /// Different lines never pair, however close they look.
    #[test]
    fn crossings_on_different_lines_never_pair() {
        assert_eq!(ends(vec![belt(0, 0, EAST), belt(4, 1, EAST)], 5), vec![UndergroundEnd::Entrance; 2]);
    }

    /// Facing matters too: two undergrounds pointing at each other are two
    /// separate crossings, not one.
    #[test]
    fn crossings_facing_differently_never_pair() {
        assert_eq!(ends(vec![belt(0, 0, EAST), belt(4, 0, WEST)], 5), vec![UndergroundEnd::Entrance; 2]);
    }

    /// Entities that are not belts are neither indexed nor written to, so a
    /// chest sitting where a feeding belt would be does not bend anything.
    #[test]
    fn only_belts_are_considered() {
        let mut entities = vec![belt(0, 0, EAST), belt(1, 0, NORTH)];
        infer_shapes(&mut entities, &[None, Some(Carrier::Belt)]);
        assert_eq!(BeltShape::from_byte(entities[1].shape), BeltShape::Straight);
    }
}
