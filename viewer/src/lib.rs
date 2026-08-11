//! Everything that doesn't touch macroquad's window/input globals, split out
//! so it's unit testable: `main.rs` is thin glue over this.
//!
//! Split by concern across these modules rather than kept as one file: each
//! one owns its own `#[cfg(test)] mod tests`, so a test lives next to the
//! code it exercises instead of in one undifferentiated block at the end.
//! Re-exported here so every name is still reachable as `viewer::Name`
//! exactly as before this split, so `main.rs` needed no import changes.

mod activity;
mod avi;
mod belts;
mod camera;
mod chrome;
mod construction;
mod draw_calls;
mod loading;
mod marks;
mod pipes;
mod player_track;
mod progress;
mod registry;
mod render_frame;
mod resample;
mod spans;
mod sprites;
mod timecode;

pub use activity::{activity_heights, analyze_activity, recent_heat, Activity, HeatCell, HEAT_CELL_TILES};
pub use avi::AviWriter;
pub use belts::{infer_shapes, infer_underground_ends, sheet_row, BeltShape, Carrier, UndergroundEnd, SHEET_ROWS};
pub use camera::{
    entity_cull_half_extents, entity_footprint_size, entity_rotation_radians, Camera, CameraTransition, Timeline,
    BASE_PIXELS_PER_TILE,
};
pub use chrome::{draw_key_panel, first_run, Chrome, ChromeState, Click, Ui};
pub use construction::{growing_bounds_per_frame, GrowingBounds};
pub use draw_calls::DrawCallCounter;
pub use loading::{
    frame_paths, group_by_surface, group_paths_by_surface, load_batch, load_frame, load_sequence, load_terrain, order_by_tick,
    synthetic_frame, synthetic_tiles, terrain_path, terrain_paths, timeline_ticks, ParallelFrameLoad,
};
pub use marks::{busy_stretches, frames_for_ticks, next_mark, previous_mark, read_bookmarks, write_bookmarks, Mark};
pub use pipes::{infer_connections, piece_name, PIECES};
pub use player_track::PlayerTrack;
pub use progress::{LoadProgress, ProgressBar};
pub use registry::{color_for, TypeId, TypeRegistry};
pub use render_frame::{
    use_chunk_lod, FrameSequence, LodCell, RenderEntity, RenderFrame, RenderTile, Run, SequenceBuilder, LOD_CELL_TILES,
    LOD_MAX_TILE_PIXELS,
};
pub use resample::downsample;
pub use spans::{Span, SpanBuilder, SpanSet};
pub use sprites::{
    belt_source_rect, entity_sheet_candidates, entity_sheet_path, icon_candidates, icon_path, icon_source_rect, pipe_piece_path,
    pipe_to_ground_paths, splitter_offsets, splitter_patch_path, splitter_source_rect, splitter_structure_paths,
    underground_source_rect, underground_structure_path, SPRITE_TILE_PIXELS,
};
pub use timecode::{format_game_time, TICKS_PER_SECOND};
