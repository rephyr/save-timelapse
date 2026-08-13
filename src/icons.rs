//! Where a prototype's icon might be found.
//!
//! Shared because both sides need the same answer for different reasons: the
//! viewer to draw an entity, and the CLI to decide whether this game has to be
//! asked to draw its icons at all. Two copies of this rule would drift, and the
//! drift would show as a timelapse that dumped icons it did not need or missed
//! ones it did.

use std::path::{Path, PathBuf};

/// Where an icon for `name` might be, best source first.
///
/// `dumped` is a timelapse's own `icons` folder, written by the game that
/// recorded it (see `export::dump_entity_icons`). It comes first because it is
/// the only source that can answer for a modded prototype: mod art cannot be
/// found by guessing, an `aai-storehouse` drawing from `container-1-base.png`
/// plus a separately tinted mask, and the runtime API exposes no icon path at
/// all.
///
/// The install's own files are the fallback, and the only source for a
/// timelapse built before icons were dumped. Vanilla and Space Age icons
/// follow a predictable path there, keyed by prototype name. A miss falls back
/// to a coloured shape rather than erroring, which is also what a modded name
/// got before.
pub fn icon_candidates(dumped: Option<&Path>, data_dir: &Path, name: &str) -> Vec<PathBuf> {
    let file = format!("{name}.png");
    let from_capture = dumped.map(|dir| dir.join(&file));
    let from_install = ["base", "space-age"].iter().map(|group| data_dir.join(group).join("graphics/icons").join(&file));
    from_capture.into_iter().chain(from_install).collect()
}

pub fn icon_path(dumped: Option<&Path>, data_dir: &Path, name: &str) -> Option<PathBuf> {
    icon_candidates(dumped, data_dir, name).into_iter().find(|candidate| candidate.exists())
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

        let path = icon_path(None, dir.path(), "stone-furnace").unwrap();
        assert_eq!(path, base_icon_dir.join("stone-furnace.png"));
    }

    /// A modded prototype has no icon under the install at all, so the
    /// capture's own dump is the only thing that can answer for it. It comes
    /// first for vanilla names too, being composited and tinted by the same
    /// game that recorded the timelapse.
    #[test]
    fn a_dumped_icon_wins_over_the_installs_own() {
        let candidates = icon_candidates(Some(Path::new("/lapse/icons")), Path::new("/data"), "kr-quarry-drill");
        assert_eq!(candidates.first(), Some(&PathBuf::from("/lapse/icons/kr-quarry-drill.png")));
        assert_eq!(candidates.len(), 3, "and the install is still tried after it");
    }

    /// A timelapse built before icons were dumped, which is every one made so
    /// far. It has to keep resolving vanilla names exactly as it did.
    #[test]
    fn icon_candidates_checks_base_then_space_age() {
        let candidates = icon_candidates(None, Path::new("/data"), "stone-furnace");
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/data/base/graphics/icons/stone-furnace.png"),
                PathBuf::from("/data/space-age/graphics/icons/stone-furnace.png"),
            ]
        );
    }
}
