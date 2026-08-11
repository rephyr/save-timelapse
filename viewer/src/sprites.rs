//! Resolving a prototype name to a Factorio icon file on disk.

use std::path::{Path, PathBuf};

use macroquad::math::Rect;

/// Vanilla/Space-Age icons follow a predictable path under the Factorio
/// install's data directory, keyed by prototype name, verified against
/// Wube's own base-game source (e.g. `__base__/graphics/icons/stone-furnace.png`
/// resolves to `<data_dir>/base/graphics/icons/stone-furnace.png`). There's no
/// runtime API a mod could use to export the exact path (checked before
/// building this), and no reliable convention for third-party mod icons:
/// a lookup miss just means falling back to a colored shape, never an error.
pub fn icon_candidates(data_dir: &Path, name: &str) -> Vec<PathBuf> {
    ["base", "space-age"].iter().map(|group| data_dir.join(group).join("graphics/icons").join(format!("{name}.png"))).collect()
}

pub fn icon_path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    icon_candidates(data_dir, name).into_iter().find(|candidate| candidate.exists())
}

/// Factorio's own in-world sheet for an entity, which for a belt holds every
/// facing and every corner drawn separately.
///
/// Same shape of guess as `icon_candidates` and the same failure mode: a miss
/// just means falling back to the inventory icon. Both groups are checked
/// because the first three belt tiers ship in `base` and turbo belts come with
/// Space Age.
pub fn entity_sheet_candidates(data_dir: &Path, name: &str) -> Vec<PathBuf> {
    ["base", "space-age"]
        .iter()
        .map(|group| data_dir.join(group).join("graphics/entity").join(name).join(format!("{name}.png")))
        .collect()
}

pub fn entity_sheet_path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    entity_sheet_candidates(data_dir, name).into_iter().find(|candidate| candidate.exists())
}

/// Factorio's structure sheet for an underground belt, which holds the
/// entrance and the exit as separate pictures.
///
/// A different file from the entity sheet a belt uses: an underground belt's
/// own folder holds `<name>-structure.png` rather than `<name>.png`.
pub fn underground_structure_path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    ["base", "space-age"]
        .iter()
        .map(|group| data_dir.join(group).join("graphics/entity").join(name).join(format!("{name}-structure.png")))
        .find(|candidate| candidate.exists())
}

/// One cell of an underground belt's structure sheet: four facings across,
/// four variants down.
///
/// Both counts come from the file, the same way the belt sheet's layout does,
/// so a tier drawn at a different resolution still lands on the right cell.
/// All four vanilla tiers measure 768x768, four 192px cells each way.
pub fn underground_source_rect(width: f32, height: f32, row: usize, column: usize) -> Rect {
    let cell_w = width / 4.0;
    let cell_h = height / 4.0;
    Rect::new(column as f32 * cell_w, row as f32 * cell_h, cell_w, cell_h)
}

/// Pixels per tile in Factorio's in-world art.
///
/// Every sheet here is authored at this density and declared `scale = 0.5` in
/// the prototypes, so a frame's size in tiles is just its pixel size over this.
/// Measured rather than assumed: a straight belt's artwork fills 68 of its
/// 128px frame, which is the 1.06 tiles a belt should be, and an underground
/// belt's housing fills 130 of 192, which is the 2.03 tiles it should be.
///
/// It matters because a frame is bigger than the thing drawn in it. Fitting
/// the frame to the entity's footprint instead draws everything at about half
/// size, with the leftover showing up as gaps between belt segments.
pub const SPRITE_TILE_PIXELS: f32 = 64.0;

/// A splitter's structure, which unlike everything else here is four separate
/// files, one per facing, rather than one sheet with the facings inside it.
///
/// Ordered north, east, south, west, matching the column order of the sheets
/// so a caller can index all of them the same way.
pub fn splitter_structure_paths(data_dir: &Path, name: &str) -> Option<Vec<PathBuf>> {
    let found: Vec<PathBuf> = ["north", "east", "south", "west"]
        .iter()
        .filter_map(|facing| {
            ["base", "space-age"]
                .iter()
                .map(|group| data_dir.join(group).join("graphics/entity").join(name).join(format!("{name}-{facing}.png")))
                .find(|candidate| candidate.exists())
        })
        .collect();
    (found.len() == 4).then_some(found)
}

/// How a splitter's animation is laid out: 32 frames as eight columns of four
/// rows, which is what the file dimensions divide into exactly for every tier
/// and facing. A still only ever wants the first.
const SPLITTER_COLUMNS: f32 = 8.0;
const SPLITTER_ROWS: f32 = 4.0;

pub fn splitter_source_rect(width: f32, height: f32) -> Rect {
    Rect::new(0.0, 0.0, width / SPLITTER_COLUMNS, height / SPLITTER_ROWS)
}

/// The top patch that completes a sideways splitter.
///
/// Facing east or west, Factorio draws a splitter in two pieces: `structure`
/// covers the near half and `structure_patch` the far half. Facing north or
/// south the patch is `util.empty_sprite()` and the structure is the whole
/// thing. Drawing only the structure is why a sideways splitter showed just
/// one of its two halves.
pub fn splitter_patch_path(data_dir: &Path, name: &str, facing: usize) -> Option<PathBuf> {
    let side = match facing {
        1 => "east",
        3 => "west",
        _ => return None,
    };
    ["base", "space-age"]
        .iter()
        .map(|group| {
            data_dir.join(group).join("graphics/entity").join(name).join(format!("{name}-{side}-top_patch.png"))
        })
        .find(|candidate| candidate.exists())
}

/// Where each piece sits relative to the entity's centre, in the units
/// Factorio's own `util.by_pixel` uses (thirty-seconds of a tile), read off
/// the `shift` in the splitter prototypes.
///
/// Ordered north, east, south, west. Base, fast and express agree on every
/// value except express's west being one unit narrower, which is a third of a
/// tenth of a tile and not worth a per-tier table.
const SPLITTER_SHIFT: [(f32, f32); 4] = [(7.0, 0.0), (4.0, 13.0), (4.0, 0.0), (6.0, 12.0)];
const SPLITTER_PATCH_SHIFT: [(f32, f32); 4] = [(0.0, 0.0), (4.0, -20.0), (0.0, 0.0), (6.0, -18.0)];

/// `util.by_pixel` divides by 32, and a tile is `SPRITE_TILE_PIXELS` of art,
/// so a shift in those units is this many pixels of the sheet.
fn by_pixel_to_sprite_pixels(value: f32) -> f32 {
    value / 32.0 * SPRITE_TILE_PIXELS
}

/// Structure and patch offsets for one facing, in sheet pixels.
pub fn splitter_offsets(facing: usize) -> ((f32, f32), (f32, f32)) {
    let s = SPLITTER_SHIFT[facing];
    let p = SPLITTER_PATCH_SHIFT[facing];
    (
        (by_pixel_to_sprite_pixels(s.0), by_pixel_to_sprite_pixels(s.1)),
        (by_pixel_to_sprite_pixels(p.0), by_pixel_to_sprite_pixels(p.1)),
    )
}

/// One of Factorio's pipe pictures, each of which is its own file rather than
/// a region of a sheet.
pub fn pipe_piece_path(data_dir: &Path, piece: &str) -> Option<PathBuf> {
    ["base", "space-age"]
        .iter()
        .map(|group| data_dir.join(group).join("graphics/entity/pipe").join(format!("{piece}.png")))
        .find(|candidate| candidate.exists())
}

/// The four pictures for an underground pipe, ordered north, east, south,
/// west so a facing indexes straight into them.
///
/// Factorio names them for the side the above-ground opening faces, following
/// the same convention as the plain pipe pieces. If every underground pipe
/// comes out facing backwards, that reading is what to flip.
pub fn pipe_to_ground_paths(data_dir: &Path) -> Option<Vec<PathBuf>> {
    let found: Vec<PathBuf> = ["up", "right", "down", "left"]
        .iter()
        .filter_map(|side| {
            ["base", "space-age"]
                .iter()
                .map(|group| {
                    data_dir.join(group).join("graphics/entity/pipe-to-ground").join(format!("pipe-to-ground-{side}.png"))
                })
                .find(|candidate| candidate.exists())
        })
        .collect();
    (found.len() == 4).then_some(found)
}

/// The source rectangle for one row of a belt sheet.
///
/// Layout is derived from the file rather than hardcoded: every belt sheet is
/// `rows` rows of square frames animated left to right, but the four tiers run
/// to different lengths (16, 32, 32 and 64 columns), so the frame size comes
/// from the height and the column count falls out of the width. Column zero
/// every time, since a timelapse frame is a still and the animation only moves
/// the belt's surface.
pub fn belt_source_rect(width: f32, height: f32, rows: usize, row: usize) -> Rect {
    let frame = height / rows as f32;
    Rect::new(0.0, row as f32 * frame, frame.min(width), frame)
}

/// Vanilla and Space Age icon files are a horizontal mipmap strip, not a
/// single image: the primary icon at full size, then progressively smaller
/// copies laid out to its right, all sharing the strip's height (verified
/// against real files, e.g. 64+32+16+8=120 wide, 64 tall). Drawing the
/// whole file stretched into one entity's box renders all of them squashed
/// together, which is what "sprites have 4 sprites in them" was.
///
/// The primary icon is always the leftmost square, sized to the image's
/// height. A single-icon file with no strip (width == height) falls out of
/// the same rule as a no-op crop, so callers don't need to special-case it.
pub fn icon_source_rect(width: f32, height: f32) -> Rect {
    let size = width.min(height);
    Rect::new(0.0, 0.0, size, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_path_prefers_the_first_existing_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let base_icon_dir = dir.path().join("base").join("graphics/icons");
        let space_age_icon_dir = dir.path().join("space-age").join("graphics/icons");
        std::fs::create_dir_all(&base_icon_dir).unwrap();
        std::fs::create_dir_all(&space_age_icon_dir).unwrap();
        std::fs::write(base_icon_dir.join("stone-furnace.png"), b"icon").unwrap();
        std::fs::write(space_age_icon_dir.join("stone-furnace.png"), b"other").unwrap();

        let path = icon_path(dir.path(), "stone-furnace").unwrap();
        assert_eq!(path, base_icon_dir.join("stone-furnace.png"));
    }

    #[test]
    fn icon_candidates_checks_base_then_space_age() {
        let candidates = icon_candidates(Path::new("/data"), "stone-furnace");
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/data/base/graphics/icons/stone-furnace.png"),
                PathBuf::from("/data/space-age/graphics/icons/stone-furnace.png"),
            ]
        );
    }

    /// Real vanilla/Space Age icon files measured directly: 64+32+16+8=120
    /// wide, 64 tall. The bug this guards against is drawing that whole
    /// strip stretched into one entity's box instead of cropping to the
    /// first (primary) icon.
    #[test]
    fn icon_source_rect_crops_a_real_mipmap_strip_to_the_primary_icon() {
        assert_eq!(icon_source_rect(120.0, 64.0), Rect::new(0.0, 0.0, 64.0, 64.0));
    }

    #[test]
    fn icon_source_rect_is_a_no_op_for_a_single_square_icon() {
        assert_eq!(icon_source_rect(64.0, 64.0), Rect::new(0.0, 0.0, 64.0, 64.0));
    }

    #[test]
    fn entity_sheet_candidates_looks_inside_a_folder_named_for_the_entity() {
        assert_eq!(
            entity_sheet_candidates(Path::new("/data"), "transport-belt"),
            vec![
                PathBuf::from("/data/base/graphics/entity/transport-belt/transport-belt.png"),
                PathBuf::from("/data/space-age/graphics/entity/transport-belt/transport-belt.png"),
            ]
        );
    }

    /// The real sheets, measured on a Factorio 2.0.77 install. All four tiers
    /// are 20 rows of 128px frames and differ only in animation length, which
    /// is exactly why the frame size is taken from the height.
    #[test]
    fn belt_source_rect_picks_a_row_from_any_tier() {
        // transport-belt: 2048x2560, 16 columns.
        assert_eq!(belt_source_rect(2048.0, 2560.0, 20, 0), Rect::new(0.0, 0.0, 128.0, 128.0));
        assert_eq!(belt_source_rect(2048.0, 2560.0, 20, 4), Rect::new(0.0, 512.0, 128.0, 128.0));
        // turbo-transport-belt: 8192x2560, 64 columns, same rows.
        assert_eq!(belt_source_rect(8192.0, 2560.0, 20, 11), Rect::new(0.0, 1408.0, 128.0, 128.0));
    }
}
