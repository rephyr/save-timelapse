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
    ["base", "space-age"]
        .iter()
        .map(|group| data_dir.join(group).join("graphics/icons").join(format!("{name}.png")))
        .collect()
}

pub fn icon_path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    icon_candidates(data_dir, name).into_iter().find(|candidate| candidate.exists())
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
}
