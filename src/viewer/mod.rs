//! Everything that does not touch macroquad's window and input globals, split
//! out so it is unit testable; `app` is thin glue over this.
//!
//! Each module owns its own `#[cfg(test)] mod tests`. Re-exported here so
//! every name is still reachable as `viewer::Name`.

pub mod app;

mod activity;
mod avi;
mod belts;
mod camera;
mod camera_path;
mod chrome;
mod construction;
mod draw_calls;
mod follow;
mod loading;
mod marks;
mod mp4;
mod pipes;
mod player_track;
mod progress;
mod rails;
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
    entity_cull_half_extents, entity_footprint_size, entity_rotation_radians, tiling_quad_size, Camera, CameraTransition,
    Framing, Timeline, BASE_PIXELS_PER_TILE,
};
pub use camera_path::{smoothing_radius, CameraPath};
pub use chrome::{draw_key_panel, first_run, Chrome, ChromeState, Click, Ui};
pub use construction::{growing_bounds_per_frame, GrowingBounds};
pub use draw_calls::DrawCallCounter;
pub use loading::{
    frame_paths, group_by_surface, group_paths_by_surface, load_batch, load_frame, load_sequence, load_terrain, order_by_tick,
    synthetic_frame, synthetic_tiles, terrain_path, terrain_paths, timeline_ticks, ParallelFrameLoad,
};
pub use marks::{busy_stretches, frames_for_ticks, next_mark, previous_mark, read_bookmarks, write_bookmarks, Mark};
pub use mp4::Mp4Writer;
pub use pipes::{infer_connections, piece_name, PIECES};
pub use player_track::PlayerTrack;
pub use progress::{LoadProgress, ProgressBar};
pub use rails::{rail_segment, RailSegment, RAIL_WIDTH_TILES};
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
