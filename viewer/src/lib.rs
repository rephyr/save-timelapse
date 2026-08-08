//! Everything that doesn't touch macroquad's window/input globals, split out
//! so it's unit testable: `main.rs` is thin glue over this.
//!
//! Split by concern across these modules rather than kept as one file: each
//! one owns its own `#[cfg(test)] mod tests`, so a test lives next to the
//! code it exercises instead of in one undifferentiated block at the end.
//! Re-exported here so every name is still reachable as `viewer::Name`
//! exactly as before this split -- `main.rs` needed no import changes.

mod camera;
mod construction;
mod draw_calls;
mod loading;
mod player_track;
mod progress;
mod registry;
mod render_frame;
mod sprites;

pub use camera::{
    entity_cull_half_extents, entity_footprint_size, entity_rotation_radians, Camera, CameraTransition,
    Timeline, BASE_PIXELS_PER_TILE,
};
pub use construction::{growing_bounds_per_frame, GrowingBounds};
pub use draw_calls::DrawCallCounter;
pub use loading::{
    frame_paths, group_by_surface, load_frame, load_sequence, load_terrain, order_by_tick,
    synthetic_frame, synthetic_tiles, terrain_path, ParallelFrameLoad,
};
pub use player_track::PlayerTrack;
pub use progress::{LoadProgress, ProgressBar};
pub use registry::{color_for, is_rotation_allowed, TypeId, TypeRegistry};
pub use render_frame::{
    use_chunk_lod, FrameSequence, LodCell, RenderEntity, RenderFrame, RenderTile, Run, LOD_CELL_TILES,
    LOD_MAX_TILE_PIXELS,
};
pub use sprites::{icon_candidates, icon_path, icon_source_rect};
