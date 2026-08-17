//! Thin macroquad glue: argument parsing, the window loop, input and
//! drawing. Everything unit testable lives in `lib.rs`, since none of it can
//! be once it touches macroquad's globals.

use std::path::PathBuf;
use std::time::Instant;

use crate::export::install_data_dir;
use crate::locate::locate_factorio;
use crate::milestone::{Kind, Milestone};
use crate::viewer::{
    activity_heights, analyze_activity, belt_source_rect, color_for, downsample, draw_key_panel, entity_cull_half_extents,
    entity_footprint_size, entity_rotation_radians, entity_sheet_path, format_game_time, growing_bounds_per_frame, icon_path,
    icon_source_rect, pipe_piece_path, pipe_to_ground_paths, recent_heat, sheet_row, splitter_offsets, splitter_patch_path,
    splitter_source_rect, splitter_structure_paths, synthetic_frame, synthetic_tiles, tiling_quad_size, underground_source_rect,
    underground_structure_path, use_chunk_lod, AviWriter, BeltShape, Camera, CameraPath, CameraTransition, Chrome, ChromeState,
    Click, DrawCallCounter, FrameSequence, Framing, GrowingBounds, HeatCell, LoadProgress, LodCell, Mp4Writer, PlayerTrack,
    ProgressBar, RailSegment, RenderEntity, RenderFrame, RenderTile, Run, Timeline, TypeId, TypeRegistry, Ui, UndergroundEnd,
    HEAT_CELL_TILES, LOD_CELL_TILES, PIECES, RAIL_WIDTH_TILES, SHEET_ROWS, SPRITE_TILE_PIXELS,
};
use macroquad::prelude::*;

const ZOOM_STEP: f32 = 1.1;
const PLAY_INTERVAL_SECS: f32 = 0.25; // ~4 frames/sec auto-play
/// Floor on how tight auto-follow can zoom. Low on purpose: it only stops the
/// very first 1x1 entity from filling the screen, and a real starter cluster
/// is already bigger than this.
const AUTO_FOLLOW_MIN_FOCUS_TILES: f32 = 6.0;
/// How much smaller than edge to edge auto-follow zooms. Tighter than
/// `fit_frames`'s own margin, following being about hugging the buildings.
const AUTO_FOLLOW_FIT_MARGIN: f32 = 0.92;
/// How long a camera move to the newly-grown extent takes, in real seconds.
/// A fixed-duration linear glide rather than an exponential approach,
/// matching TLBE. See `Camera::CameraTransition`.
const AUTO_FOLLOW_TRANSITION_SECS: f32 = 1.5;
/// Share of the frame height kept clear along the bottom when following.
///
/// The scrub bar, its activity graph and its labels live down there, and a
/// base fitted edge to edge puts its southern buildings behind them. A share
/// rather than a pixel count so an export is framed like the window that
/// previewed it, whatever either one's size.
const AUTO_FOLLOW_BOTTOM_INSET: f32 = 0.1;
/// Below this, a sprite is imperceptible and not worth a texture draw over a
/// flat rect: the zoom-based sprites/shapes split agreed back in the
/// milestone-1 discussion.
const SPRITE_MIN_PIXELS: f32 = 12.0;

/// macroquad starts a new draw call when its batch fills, so the default caps
/// one at 833 quads. Not higher than 4,096: indices are `u16` offset by the
/// running vertex count, so capacity past 65,536 corrupts geometry, and one
/// buffer of this size is allocated per draw call ever used.
const BATCH_QUAD_CAPACITY: usize = 4096;
const BATCH_VERTEX_CAPACITY: usize = BATCH_QUAD_CAPACITY * 4;
const BATCH_INDEX_CAPACITY: usize = BATCH_QUAD_CAPACITY * 6;

/// Oversampling an export uses unless told otherwise. 2 costs four times the
/// pixels and is where the averaging is clearly visible.
const DEFAULT_SUPERSAMPLE: u32 = 2;

/// How long an exported camera move takes unless told otherwise. Matches
/// `AUTO_FOLLOW_TRANSITION_SECS` so a video is paced like browsing the same
/// capture, even though the two get there by different means.
const DEFAULT_SMOOTH_SECS: f32 = 1.5;

/// Longest render target edge to ask a GPU for. Past roughly this, drivers
/// start refusing the allocation, and macroquad reports that as a texture
/// that simply never draws rather than as an error.
const MAX_RENDER_EDGE: u32 = 8192;

/// Frame files parsed at once before being folded into spans and dropped.
/// Bounds peak load memory at roughly this many frames, while still being
/// wide enough to keep every core busy parsing (see `load_batch`).
const LOAD_BATCH_FRAMES: usize = 16;

/// How often loading pauses to draw the progress bar. Yielding per item would
/// pace loading to the display's refresh rate; this keeps the bar responsive
/// while leaving loading effectively at full speed.
const PROGRESS_REDRAW: std::time::Duration = std::time::Duration::from_millis(33);

pub fn window_conf() -> macroquad::conf::Conf {
    macroquad::conf::Conf {
        miniquad_conf: miniquad::conf::Conf {
            window_title: "save-timelapse viewer".to_owned(),
            window_width: 1280,
            window_height: 800,
            ..Default::default()
        },
        draw_call_vertex_capacity: BATCH_VERTEX_CAPACITY,
        draw_call_index_capacity: BATCH_INDEX_CAPACITY,
        ..Default::default()
    }
}

/// Everything the draw loop tracks per world: playback position, camera and
/// auto-follow state.
struct WorldView {
    name: String,
    sequence: FrameSequence,
    camera: Camera,
    /// The whole base's bounding box as of each frame, monotonically
    /// growing, precomputed once at load. See `crate::viewer::growing_bounds_per_frame`.
    growing_bounds: Vec<Option<GrowingBounds>>,
    /// Everything this surface ever contains, terrain included: the box the
    /// opening camera is fitted to. Kept because an export smooths its way out
    /// of it rather than cutting, and rediscovering it means another walk over
    /// every entity of every frame.
    opening_bounds: Option<GrowingBounds>,
    /// How much got built in each frame, normalized to 0..1. Precomputed at
    /// load, since recovering it needs a diff between consecutive frames.
    activity: Vec<f32>,
    /// Where construction happened in each frame, binned into cells for the
    /// heatmap overlay. Same pass as `activity` above.
    heat: Vec<Vec<HeatCell>>,
    /// The busiest single cell of the run, so heat brightness stays anchored
    /// to a fixed reference rather than to whatever is currently on screen.
    heat_peak: u32,
    /// Frames worth jumping to with `[` and `]`: every milestone, plus
    /// whatever the viewer has bookmarked. Sorted, and rebuilt whenever a
    /// bookmark is added or removed.
    jump_targets: Vec<usize>,
    /// The busiest frame of each stretch of sustained construction, for
    /// PageUp and PageDown. Fixed at load, since the activity it derives
    /// from is.
    busy: Vec<usize>,
    /// Just the bookmarked frames, for drawing them on the bar. Kept beside
    /// `jump_targets` rather than derived from it, because that list has
    /// milestones mixed in and they are already drawn their own way.
    bookmark_frames: Vec<usize>,
    /// Bookmarked *ticks*, not frames. See `crate::viewer::marks`: a rebuild at a
    /// different seconds-per-frame renumbers every index, so a bookmark that
    /// meant an index would silently come back pointing somewhere else.
    bookmarks: Vec<u64>,
    follow: FollowState,
    /// This surface's terrain layer, loaded once: terrain never changes after
    /// the baseline. `None` when terrain capture was off.
    terrain: Option<RenderFrame>,
}

#[derive(Default)]
struct FollowState {
    enabled: bool,
    /// Whichever bounds the current (or last finished) transition is/was
    /// headed towards, so a new one only starts once the tracked area
    /// actually grows, not on every rendered frame.
    target_bounds: Option<GrowingBounds>,
    /// The in-flight move to `target_bounds`, left to finish rather than
    /// restarted whenever the tracked area grows: during active building it
    /// grows almost every frame, so retargeting chases a stale target.
    transition: Option<CameraTransition>,
}

/// Loop-scoped UI state that isn't per-world (unlike `WorldView`'s fields):
/// playback, the sprite/LOD toggles, and drag tracking.
struct ViewerState {
    last_mouse: Vec2,
    playing: bool,
    play_accum: f32,
    play_speed: f32,
    dragging_timeline: bool,
    /// Latched on press when the click landed on a control, so the drag that
    /// follows moves the button's own state rather than the camera behind it.
    on_chrome: bool,
    sprites_enabled: bool,
    lod_enabled: bool,
    heatmap_enabled: bool,
    /// Whether player markers are drawn. Unlike the two above it survives the
    /// session, so it is written back whenever it changes.
    players_enabled: bool,
    /// Whether the `+N more` surface list is open.
    surfaces_expanded: bool,
}

/// Render every frame to an image file instead of opening for browsing.
/// Resolution is independent of the window, so output does not depend on how
/// somebody dragged a corner.
struct ExportRequest {
    dir: PathBuf,
    width: u32,
    height: u32,
    /// Frames per second when writing video. Ignored for an image sequence,
    /// which has no notion of a rate.
    fps: u32,
    /// Write a playable video instead of a folder of PNGs.
    video: bool,
    /// Burn player markers into the exported frames.
    overlay_players: bool,
    /// Burn the in-game clock into the exported frames.
    overlay_clock: bool,
    /// H.264 in an MP4 through FFmpeg rather than the built-in MJPEG AVI.
    /// Only ever set when the CLI found FFmpeg, the tool's own writer being
    /// what keeps it dependency free.
    mp4: bool,
    /// Which surface to render. `None` means the busiest, which is what
    /// `group_by_surface` already orders first. `Some("all")` renders every
    /// surface into its own subfolder.
    surface: Option<String>,
    /// Render this many times oversized on each axis, then average down. At
    /// 1080p a 2,900 tile base puts three tiles behind every pixel, so whichever
    /// entity a pixel lands on wins it outright. This is also what makes full
    /// detail worth asking for in an export.
    supersample: u32,
    /// Roughly how long the camera takes to complete a move, in seconds of
    /// finished video. Zero fits every frame's own bounds exactly, which is
    /// what this used to do and what snapped. See `crate::viewer::camera_path`.
    smooth_secs: f32,
}

struct Args {
    path: Option<String>,
    synthetic_entities: Option<usize>,
    synthetic_tile_count: Option<usize>,
    factorio: Option<PathBuf>,
    export: Option<ExportRequest>,
}

fn parse_args(args: &[String]) -> Args {
    let mut result = Args { path: None, synthetic_entities: None, synthetic_tile_count: None, factorio: None, export: None };
    let (mut export_dir, mut width, mut height) = (None, 1920u32, 1080u32);
    let mut supersample = DEFAULT_SUPERSAMPLE;
    let mut surface = None;
    let (mut fps, mut video, mut mp4) = (30u32, false, false);
    let (mut overlay_players, mut overlay_clock) = (false, false);
    let mut smooth_secs = DEFAULT_SMOOTH_SECS;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--synthetic" => {
                i += 1;
                result.synthetic_entities = Some(args.get(i).and_then(|s| s.parse().ok()).unwrap_or(500_000));
            }
            "--synthetic-tiles" => {
                i += 1;
                result.synthetic_tile_count = Some(args.get(i).and_then(|s| s.parse().ok()).unwrap_or(500_000));
            }
            "--factorio" => {
                i += 1;
                result.factorio = args.get(i).map(PathBuf::from);
            }
            "--export" => {
                i += 1;
                export_dir = args.get(i).map(PathBuf::from);
            }
            "--width" => {
                i += 1;
                width = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(width);
            }
            "--height" => {
                i += 1;
                height = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(height);
            }
            "--surface" => {
                i += 1;
                surface = args.get(i).cloned();
            }
            "--fps" => {
                i += 1;
                fps = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(fps);
            }
            "--supersample" => {
                i += 1;
                supersample = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(supersample);
            }
            "--smooth" => {
                i += 1;
                smooth_secs = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(smooth_secs);
            }
            "--video" => video = true,
            "--mp4" => mp4 = true,
            "--overlay-players" => overlay_players = true,
            "--overlay-clock" => overlay_clock = true,
            other => result.path = Some(other.to_string()),
        }
        i += 1;
    }

    // Clamped rather than trusted: a zero or negative size would make an
    // unusable render target, and something enormous would exhaust GPU memory
    // partway through a long export, which is a far worse way to find out.
    result.export = export_dir.map(|dir| {
        // Rounded down to even: JPEG works in 8x8 blocks and some decoders
        // are fussy about odd dimensions, which is a miserable thing to
        // discover only when a player refuses the finished file.
        let width = width.clamp(160, 7680) & !1;
        let height = height.clamp(120, 4320) & !1;
        ExportRequest {
            dir,
            width,
            height,
            surface,
            fps: fps.clamp(1, 240),
            video,
            mp4,
            overlay_players,
            overlay_clock,
            // Capped by the render target it implies: GPUs stop honouring
            // texture sizes past 8192 a side, and a refused target is a black
            // export rather than an error.
            supersample: supersample.clamp(1, 4).min(MAX_RENDER_EDGE / width).min(MAX_RENDER_EDGE / height).max(1),
            // Capped at ten seconds: past that the filter reaches so far ahead
            // that the camera is framing a factory that will not exist for
            // another half minute, which stops looking like anticipation and
            // starts looking like it is pointed at nothing.
            smooth_secs: smooth_secs.clamp(0.0, 10.0),
        }
    });

    result
}

// Loading, with a progress bar

fn draw_loading(progress: &LoadProgress) {
    clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

    let bar = ProgressBar::centered(screen_width(), screen_height());
    draw_rectangle(bar.left, bar.top, bar.width, bar.height, Color::new(1.0, 1.0, 1.0, 0.12));
    draw_rectangle(bar.left, bar.top, bar.filled_width(progress), bar.height, Color::new(0.45, 0.75, 1.0, 0.9));
    draw_rectangle_lines(bar.left, bar.top, bar.width, bar.height, 2.0, Color::new(1.0, 1.0, 1.0, 0.35));

    let headline = if progress.total > 0 {
        format!("{} {}/{}", progress.phase, progress.done, progress.total)
    } else {
        progress.phase.to_string()
    };
    draw_text_legible(&headline, bar.left, bar.top - 14.0, 24.0, WHITE);
    if !progress.detail.is_empty() {
        draw_text_legible(&progress.detail, bar.left, bar.top + bar.height + 26.0, 18.0, Color::new(1.0, 1.0, 1.0, 0.85));
    }
}

/// The export's own progress, in the window that would otherwise stay black.
///
/// Drawn after `set_default_camera`, which is what hands painting back from
/// the render target every frame is composed into.
fn draw_export_progress(done: usize, total: usize, destination: &str) {
    let progress = LoadProgress { phase: "rendering", detail: destination.to_string(), done, total };
    draw_loading(&progress);
}

/// Draw the bar and yield to the window, but only if enough time has passed
/// since the last redraw, so a fast load doesn't pay a vsync wait per item.
async fn redraw_progress(progress: &LoadProgress, last: &mut Instant, force: bool) {
    if !force && last.elapsed() < PROGRESS_REDRAW {
        return;
    }
    *last = Instant::now();
    draw_loading(progress);
    next_frame().await;
}

/// Load frames, interning names as we go, showing progress throughout.
///
/// `--synthetic-tiles` is independent of `--synthetic`, layering a synthetic
/// floor over whichever entities were requested: the tile-heavy case the entity
/// stress test does not cover.
async fn load_frames(args: &Args, registry: &mut TypeRegistry) -> Vec<(String, FrameSequence, Option<RenderFrame>)> {
    let mut last = Instant::now();
    let mut progress = LoadProgress { phase: "reading frames", detail: String::new(), done: 0, total: 0 };

    // Single-frame paths (synthetic, or the default fixture) share the tail
    // of this function; only a real directory can have more than one world.
    let single = if let Some(n) = args.synthetic_entities {
        progress.phase = "building synthetic frame";
        progress.detail = format!("{n} entities");
        redraw_progress(&progress, &mut last, true).await;
        Some(synthetic_frame(n))
    } else if args.path.is_none() {
        let default = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames/frame_0004.stfr");
        println!("no frame given, defaulting to {default}");
        progress.detail = "default fixture".to_string();
        redraw_progress(&progress, &mut last, true).await;
        let bytes = std::fs::read(default).expect("failed to read default fixture");
        Some(crate::frame::read_binary(&bytes).expect("failed to parse frame"))
    } else {
        None
    };

    let mut result: Vec<(String, FrameSequence, Option<RenderFrame>)> = Vec::new();

    if let Some(mut frame) = single {
        if let Some(n) = args.synthetic_tile_count {
            frame.tiles = synthetic_tiles(n);
        }
        let name = frame.surface.clone();
        let mut builder = FrameSequence::builder();
        builder.push(&RenderFrame::from_frame(frame, registry));
        // Synthetic/default-fixture loads have no on-disk directory to look
        // for a terrain file in, and nothing produces one for them.
        if let Some(sequence) = builder.finish(registry) {
            result.push((name, sequence, None));
        }
    } else {
        let path = args.path.as_ref().expect("checked above");
        println!("loading {path}");
        let path = std::path::Path::new(path);
        let paths = crate::viewer::frame_paths(path).expect("failed to enumerate frames");

        // A single frame file's terrain (if it even has one) would sit beside
        // it in its parent directory, same as a real capture's.
        let terrain_dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        let terrain_file_paths = crate::viewer::terrain_paths(terrain_dir).unwrap_or_default();

        // Terrain starts loading before the frames: two waits back to back
        // cost their sum, started together they cost the slower.
        progress.total = paths.len() + terrain_file_paths.len();
        progress.detail = format!("{} core(s)", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
        let terrain_load = crate::viewer::ParallelFrameLoad::start(terrain_file_paths);

        // Grouped from headers rather than parsed frames, which is what makes
        // the streaming below possible: surface and tick fix the order, at a
        // bounded read per file.
        progress.phase = "reading frame headers";
        redraw_progress(&progress, &mut last, true).await;
        let grouped = crate::viewer::group_paths_by_surface(paths);
        // Every moment the export covers. A surface's file is omitted at a
        // moment nothing on it changed, so no single surface describes the
        // whole timeline. See `crate::viewer::timeline_ticks`.
        let timeline = crate::viewer::timeline_ticks(&grouped);

        // Painted before the first batch rather than after it. Setting a phase
        // and not redrawing until the end of the work leaves the previous
        // phase's label on screen for the whole of it, so a slow batch reads
        // as a stuck header scan and sends anyone diagnosing it to the wrong
        // function entirely.
        progress.phase = "loading frames";
        redraw_progress(&progress, &mut last, true).await;
        let mut done = 0usize;
        for (name, paths) in grouped {
            let mut builder = FrameSequence::builder();
            // How far through `timeline` this surface has been filled in.
            let mut filled = 0usize;
            let only_paths: Vec<std::path::PathBuf> = paths.iter().map(|(_, p)| p.clone()).collect();
            for chunk in only_paths.chunks(LOAD_BATCH_FRAMES) {
                // One batch is parsed across every core, folded into spans,
                // and dropped before the next is read, so peak memory is a
                // batch plus the spans rather than the whole capture.
                for mut frame in crate::viewer::load_batch(chunk) {
                    if let Some(n) = args.synthetic_tile_count {
                        frame.tiles = synthetic_tiles(n);
                    }
                    // Put back the moments this surface sat unchanged, so the
                    // index-addressed timeline means the same thing on every
                    // surface. Keyed on the parsed tick, `load_batch` dropping
                    // a file it cannot parse.
                    if let Some(offset) = timeline[filled..].iter().position(|&t| t == frame.tick) {
                        builder.push_repeats(&timeline[filled..filled + offset]);
                        filled += offset + 1;
                    }
                    builder.push(&RenderFrame::from_frame(frame, registry));
                }
                done += chunk.len();
                progress.done = done + terrain_load.done();
                redraw_progress(&progress, &mut last, false).await;
            }
            // A surface that stopped changing before the capture ended still
            // exists for the rest of it.
            builder.push_repeats(&timeline[filled..]);
            if let Some(sequence) = builder.finish(registry) {
                result.push((name, sequence, None));
            }
        }

        progress.phase = "loading terrain";
        let loaded_terrain = loop {
            progress.done = done + terrain_load.done();
            redraw_progress(&progress, &mut last, true).await;
            if let Some(loaded) = terrain_load.poll() {
                break loaded;
            }
        };
        let mut terrain_by_surface: std::collections::HashMap<String, crate::frame::Frame> =
            loaded_terrain.into_iter().map(|frame| (frame.surface.clone(), frame)).collect();
        for (name, _, terrain) in &mut result {
            *terrain = terrain_by_surface.remove(name).map(|frame| RenderFrame::from_frame(frame, registry));
        }
    }

    redraw_progress(&progress, &mut last, true).await;

    println!(
        "{} world(s) loaded ({} frame(s) total), {} distinct type(s)",
        result.len(),
        result.iter().map(|(_, seq, _)| seq.len()).sum::<usize>(),
        registry.len()
    );
    result
}

/// A loaded icon plus the region of it that is the actual icon. Icon files are
/// a mipmap strip, so drawing the whole texture renders every copy squashed
/// together; `icon_rect` crops to the first.
struct Sprite {
    /// Usually one. A splitter has four, one per facing, because that is how
    /// Factorio ships them: `splitter-north.png` and its three siblings rather
    /// than one sheet with the facings inside it.
    textures: Vec<Texture2D>,
    icon_rect: Rect,
    /// Which of Factorio's own in-world sheets these are, if any. `None` means
    /// an ordinary inventory icon, drawn the way it always was.
    sheet: Option<SheetKind>,
    /// Per-facing offset from the entity's centre, in tiles. Only a splitter
    /// has these, because only a splitter is assembled from pieces whose own
    /// shifts move where the middle ends up.
    splitter_offsets: Vec<Vec2>,
}

impl Sprite {
    fn primary(&self) -> &Texture2D {
        &self.textures[0]
    }
}

/// The two sheet layouts worth reading, both of which hold several pictures of
/// one entity and need the right one picked per entity rather than per type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SheetKind {
    /// 20 rows of square frames: four facings, eight corners, eight end caps.
    Belt,
    /// Four facings across by four variants down: exit, entrance, and the two
    /// side-loading forms this does not use.
    UndergroundStructure,
    /// One file per facing, each 32 frames as eight columns of four rows.
    Splitter,
    /// Sixteen files, one per combination of the four sides something joins
    /// onto, indexed by the connection mask.
    Pipe,
    /// Four files, one per facing.
    PipeToGround,
}

/// Where in `sprite` to read one entity's picture, and how far to rotate it. A
/// belt from the in-world sheet is never rotated, every facing and corner being
/// a separate frame Factorio drew the right way up.
struct EntityArt {
    /// Index into `Sprite::textures`. Always 0 except for a splitter, whose
    /// facings are four separate files.
    texture: usize,
    source: Rect,
    rotation: f32,
    flip_x: bool,
    flip_y: bool,
    /// How many tiles the chosen frame covers, when that is not the entity's
    /// footprint: Factorio's frames are bigger than the thing inside them, so
    /// fitting frame to footprint draws everything at roughly half size.
    tiles: Option<Vec2>,
    /// Where the frame sits relative to the entity's centre, in tiles.
    /// Factorio's sprites carry a `shift`, and a splitter's is about a fifth
    /// of a tile sideways.
    offset: Vec2,
}

impl EntityArt {
    fn plain(source: Rect, rotation: f32) -> EntityArt {
        EntityArt { texture: 0, source, rotation, flip_x: false, flip_y: false, tiles: None, offset: Vec2::ZERO }
    }

    /// A rail piece drawn along the path it occupies rather than on the tile
    /// its centre sits in. Always a flat colour, never the icon: a rail's icon
    /// is an oblique render of a short piece of track, so rotating it to a
    /// heading spins the camera angle rather than the track.
    fn rail(segment: RailSegment) -> EntityArt {
        EntityArt {
            texture: 0,
            source: Rect::default(),
            rotation: segment.rotation,
            flip_x: false,
            flip_y: false,
            tiles: Some(Vec2::new(segment.length, RAIL_WIDTH_TILES)),
            // A curve half is not centred on the position it is recorded at,
            // unlike a straight, so the run is shifted onto where it really
            // sits rather than drawn around the wrong point.
            offset: Vec2::new(segment.offset.0, segment.offset.1),
        }
    }

    /// A frame drawn at its own size, worked out from its pixels.
    fn sized(texture: usize, source: Rect) -> EntityArt {
        EntityArt {
            texture,
            source,
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            tiles: Some(Vec2::new(source.w, source.h) / SPRITE_TILE_PIXELS),
            offset: Vec2::ZERO,
        }
    }
}

fn entity_source(sprite: &Sprite, entity: &RenderEntity, rotation_allowed: bool) -> EntityArt {
    let (width, height) = (sprite.primary().width(), sprite.primary().height());
    match sprite.sheet {
        Some(SheetKind::Belt) => {
            if let Some(row) = sheet_row(entity.d, BeltShape::from_byte(entity.shape)) {
                return EntityArt::sized(0, belt_source_rect(width, height, SHEET_ROWS, row));
            }
        }
        // One file per facing, so the facing picks the texture rather than a
        // region inside one. Frame zero of the animation: a timelapse frame is
        // a still, and the other 31 only move the belt surface.
        Some(SheetKind::PipeToGround) if entity.d.is_multiple_of(4) => {
            let facing = (entity.d / 4) as usize;
            let texture = &sprite.textures[facing];
            return EntityArt::sized(facing, Rect::new(0.0, 0.0, texture.width(), texture.height()));
        }
        Some(SheetKind::PipeToGround) => {}
        // The mask indexes straight into the textures, which were loaded in
        // the same order (see `pipes::PIECES`).
        Some(SheetKind::Pipe) => {
            let piece = (entity.shape & 0b1111) as usize;
            let texture = &sprite.textures[piece];
            return EntityArt::sized(piece, Rect::new(0.0, 0.0, texture.width(), texture.height()));
        }
        // Already assembled and already frame zero, so the whole texture is
        // the picture. The offset is the composite's own, which is not either
        // piece's shift once two differently placed halves are joined.
        Some(SheetKind::Splitter) if entity.d.is_multiple_of(4) => {
            let facing = (entity.d / 4) as usize;
            let texture = &sprite.textures[facing];
            let source = Rect::new(0.0, 0.0, texture.width(), texture.height());
            return EntityArt { offset: sprite.splitter_offsets[facing], ..EntityArt::sized(facing, source) };
        }
        Some(SheetKind::Splitter) => {}
        // Columns run north, east, south, west, so the 16-way byte divides
        // straight down to a column.
        //
        // The exit is the entrance mirrored along the direction items travel:
        // the two structures differ only across a 67x69 patch of a 192px cell,
        // so taking them at face value leaves a crossing looking like the same
        // object twice.
        Some(SheetKind::UndergroundStructure) if entity.d.is_multiple_of(4) => {
            let end = UndergroundEnd::from_byte(entity.shape);
            let mirrored = end == UndergroundEnd::Exit;
            let along_x = entity.d == 4 || entity.d == 12;
            let source = underground_source_rect(width, height, end.sheet_row(), (entity.d / 4) as usize);
            return EntityArt { flip_x: mirrored && along_x, flip_y: mirrored && !along_x, ..EntityArt::sized(0, source) };
        }
        Some(SheetKind::UndergroundStructure) => {}
        None => {}
    }
    let rotation = entity_rotation_radians(entity.w as u32, entity.h as u32, entity.d, rotation_allowed);
    EntityArt::plain(sprite.icon_rect, rotation)
}

/// One splitter facing, assembled into a single picture. Facing east or west
/// Factorio draws a splitter in two pieces, joined once at load rather than as
/// a second quad per entity, which would break the per-type batch. The
/// composite's offset comes back too, joining two shifted pieces moving the
/// middle.
struct SplitterFacing {
    texture: Texture2D,
    /// Offset from the entity's centre, in tiles.
    offset: Vec2,
}

/// Combines a splitter's structure with its top patch, if it has one. Worked
/// out in sheet pixels and converted to tiles at the end, that being the space
/// Factorio's shifts are quoted in.
fn assemble_splitter(structure: &Image, patch: Option<&Image>, facing: usize) -> SplitterFacing {
    let frame = |image: &Image| {
        let rect = splitter_source_rect(image.width() as f32, image.height() as f32);
        (rect.w, rect.h)
    };
    let (structure_offset, patch_offset) = splitter_offsets(facing);
    let (sw, sh) = frame(structure);

    // Each piece's extent around the entity centre, then the union of them.
    let mut min = Vec2::new(structure_offset.0 - sw / 2.0, structure_offset.1 - sh / 2.0);
    let mut max = Vec2::new(structure_offset.0 + sw / 2.0, structure_offset.1 + sh / 2.0);
    if let Some(patch) = patch {
        let (pw, ph) = frame(patch);
        min = min.min(Vec2::new(patch_offset.0 - pw / 2.0, patch_offset.1 - ph / 2.0));
        max = max.max(Vec2::new(patch_offset.0 + pw / 2.0, patch_offset.1 + ph / 2.0));
    }

    let size = max - min;
    let mut canvas = Image::gen_image_color(size.x as u16, size.y as u16, Color::new(0.0, 0.0, 0.0, 0.0));
    // Patch first: it is the far half, so the near half draws over it where
    // they meet, which is the order Factorio layers them in.
    if let Some(patch) = patch {
        let (pw, ph) = frame(patch);
        let at = Vec2::new(patch_offset.0 - pw / 2.0, patch_offset.1 - ph / 2.0) - min;
        blit(&mut canvas, patch, at);
    }
    let at = Vec2::new(structure_offset.0 - sw / 2.0, structure_offset.1 - sh / 2.0) - min;
    blit(&mut canvas, structure, at);

    let texture = Texture2D::from_image(&canvas);
    texture.set_filter(FilterMode::Linear);
    SplitterFacing { texture, offset: (min + max) / 2.0 / SPRITE_TILE_PIXELS }
}

/// Copies frame zero of `source` onto `canvas` at `at`, keeping whatever is
/// already there wherever the source is transparent.
fn blit(canvas: &mut Image, source: &Image, at: Vec2) {
    let rect = splitter_source_rect(source.width() as f32, source.height() as f32);
    for y in 0..rect.h as u32 {
        for x in 0..rect.w as u32 {
            let pixel = source.get_pixel(x, y);
            if pixel.a <= 0.0 {
                continue;
            }
            let (tx, ty) = (at.x as u32 + x, at.y as u32 + y);
            if tx < canvas.width() as u32 && ty < canvas.height() as u32 {
                canvas.set_pixel(tx, ty, pixel);
            }
        }
    }
}

/// Sprites indexed by `TypeId`, so drawing never hashes a name. Best-effort:
/// this only covers vanilla and Space Age naming, and a missing icon just
/// means that type keeps its coloured shape.
async fn load_sprites(
    dumped_icons: Option<&std::path::Path>,
    data_dir: Option<&std::path::Path>,
    registry: &TypeRegistry,
) -> Vec<Option<Sprite>> {
    let mut sprites: Vec<Option<Sprite>> = (0..registry.len()).map(|_| None).collect();
    // The in-world sheets belts and pipes draw from live in the install, so
    // without one there is nothing to draw those with. Dumped icons are
    // self-contained, though, so a timelapse carrying them still shows its
    // factory on a machine that has never had Factorio on it.
    let data_dir = match (data_dir, dumped_icons) {
        (Some(dir), _) => dir,
        (None, Some(_)) => std::path::Path::new(""),
        (None, None) => return sprites,
    };

    let mut last = Instant::now();
    let mut progress = LoadProgress { phase: "loading sprites", detail: String::new(), done: 0, total: registry.len() };

    for (id, name) in registry.names().iter().enumerate() {
        let type_id = id as TypeId;
        progress.done = id;
        progress.detail = name.clone();
        redraw_progress(&progress, &mut last, false).await;

        // Belts come from the in-world sheet, the only place corner artwork
        // exists. Splitters are assembled, each facing being one or two files.
        if registry.is_splitter(type_id) {
            if let Some(paths) = splitter_structure_paths(data_dir, name) {
                let mut facings = Vec::with_capacity(4);
                for (facing, path) in paths.iter().enumerate() {
                    let Some(path) = path.to_str() else { break };
                    let Ok(structure) = load_image(path).await else { break };
                    let mut patch = None;
                    if let Some(found) = splitter_patch_path(data_dir, name, facing) {
                        if let Some(found) = found.to_str() {
                            patch = load_image(found).await.ok();
                        }
                    }
                    facings.push(assemble_splitter(&structure, patch.as_ref(), facing));
                }
                if facings.len() == 4 {
                    let icon_rect = Rect::new(0.0, 0.0, facings[0].texture.width(), facings[0].texture.height());
                    sprites[id] = Some(Sprite {
                        splitter_offsets: facings.iter().map(|f| f.offset).collect(),
                        textures: facings.into_iter().map(|f| f.texture).collect(),
                        icon_rect,
                        sheet: Some(SheetKind::Splitter),
                    });
                    continue;
                }
            }
        }

        if registry.is_pipe_to_ground(type_id) {
            if let Some(paths) = pipe_to_ground_paths(data_dir) {
                let mut textures = Vec::with_capacity(4);
                for path in &paths {
                    let Some(path) = path.to_str() else { break };
                    let Ok(texture) = load_texture(path).await else { break };
                    textures.push(texture);
                }
                if textures.len() == 4 {
                    let icon_rect = Rect::new(0.0, 0.0, textures[0].width(), textures[0].height());
                    sprites[id] =
                        Some(Sprite { textures, icon_rect, sheet: Some(SheetKind::PipeToGround), splitter_offsets: Vec::new() });
                    continue;
                }
            }
        }

        // A pipe needs all sixteen of its pictures, since which one it draws
        // depends on its neighbours and changes frame to frame.
        if registry.is_pipe(type_id) {
            let mut textures = Vec::with_capacity(PIECES.len());
            for piece in PIECES {
                let Some(path) = pipe_piece_path(data_dir, piece) else { break };
                let Some(path) = path.to_str().map(str::to_owned) else { break };
                let Ok(texture) = load_texture(&path).await else { break };
                textures.push(texture);
            }
            if textures.len() == PIECES.len() {
                let icon_rect = Rect::new(0.0, 0.0, textures[0].width(), textures[0].height());
                sprites[id] = Some(Sprite { textures, icon_rect, sheet: Some(SheetKind::Pipe), splitter_offsets: Vec::new() });
                continue;
            }
        }

        let found = if registry.is_belt(type_id) {
            entity_sheet_path(data_dir, name).map(|path| (vec![path], SheetKind::Belt))
        } else if registry.underground_reach(type_id).is_some() {
            underground_structure_path(data_dir, name).map(|path| (vec![path], SheetKind::UndergroundStructure))
        } else {
            None
        };
        let sheet = found.as_ref().map(|(_, kind)| *kind);
        let paths = found.map(|(paths, _)| paths).or_else(|| icon_path(dumped_icons, data_dir, name).map(|p| vec![p]));

        // All or nothing: a splitter missing one facing would index past the
        // end at draw time, so the whole type falls back to its icon.
        if let Some(paths) = paths {
            let mut textures = Vec::with_capacity(paths.len());
            for path in &paths {
                match path.to_str().map(str::to_owned) {
                    Some(path) => match load_texture(&path).await {
                        Ok(texture) => textures.push(texture),
                        Err(_) => break,
                    },
                    None => break,
                }
            }
            if textures.len() == paths.len() {
                let icon_rect = icon_source_rect(textures[0].width(), textures[0].height());
                sprites[id] = Some(Sprite { textures, icon_rect, sheet, splitter_offsets: Vec::new() });
            }
        }
    }

    progress.done = registry.len();
    redraw_progress(&progress, &mut last, true).await;
    sprites
}

// Drawing

fn draw_entity(center: Vec2, size: Vec2, color: Color, sprite: Option<&Sprite>, art: &EntityArt) {
    // Centred, so a frame that covers more than its footprint spreads evenly
    // around the tile it sits on rather than off one corner. `size` already
    // accounts for `art.tiles`; see the call site, which is where the zoom is.
    let top_left = center - size / 2.0;
    match sprite {
        Some(sprite) => draw_texture_ex(
            &sprite.textures[art.texture],
            top_left.x,
            top_left.y,
            WHITE,
            DrawTextureParams {
                dest_size: Some(size),
                source: Some(art.source),
                rotation: art.rotation,
                flip_x: art.flip_x,
                flip_y: art.flip_y,
                ..Default::default()
            },
        ),
        None => draw_rectangle_ex(
            center.x,
            center.y,
            size.x,
            size.y,
            DrawRectangleParams { rotation: art.rotation, offset: Vec2::splat(0.5), color },
        ),
    }
}

/// Tiles are corner positioned, unlike entities, so `screen` here is the
/// tile's top-left corner rather than its center.
fn draw_tile(screen: Vec2, size: f32, color: Color) {
    draw_rectangle(screen.x, screen.y, size, size, color);
}

/// One level-of-detail cell: always a flat rect, never a sprite. A chunk
/// aggregates several types into "whichever is dominant," so no single icon
/// would be honest about what's actually there.
fn draw_lod_cell(screen: Vec2, size: f32, color: Color) {
    draw_rectangle(screen.x, screen.y, size, size, color);
}

/// World-space view rectangle, so culling is a pair of comparisons per item
/// rather than a transform and a screen bounds test.
///
/// Sized from `screen_center` doubled rather than `screen_width()`, which is
/// the window: an export renders offscreen, and culling to the window cropped
/// every 1080p export to a window-sized corner.
fn view_bounds(camera: &Camera, screen_center: Vec2) -> (Vec2, Vec2) {
    let min = camera.screen_to_world(Vec2::ZERO, screen_center);
    let max = camera.screen_to_world(screen_center * 2.0, screen_center);
    (min, max)
}

/// Full-detail tile drawing, extracted so it can run twice: once for a
/// terrain backdrop (if loaded), once for the current frame's own placed
/// floor drawn on top of it, matching how paving over grass looks in-game.
#[allow(clippy::too_many_arguments)]
fn draw_tile_layer(
    tiles: &[RenderTile],
    tile_runs: &[Run],
    camera: &Camera,
    screen_center: Vec2,
    view_min: Vec2,
    view_max: Vec2,
    registry: &TypeRegistry,
    counter: &mut DrawCallCounter,
) {
    let tile_size = tiling_quad_size(camera.pixels_per_tile(), 1.0);
    for run in tile_runs {
        let color = registry.tile_color(run.type_id);
        let mut drawn = 0;
        for tile in &tiles[run.range()] {
            // A tile at (x,y) covers [x,x+1) x [y,y+1).
            let (x, y) = (tile.x as f32, tile.y as f32);
            if x + 1.0 < view_min.x || x > view_max.x || y + 1.0 < view_min.y || y > view_max.y {
                continue;
            }
            let screen = camera.world_to_screen(Vec2::new(x, y), screen_center);
            draw_tile(screen, tile_size, color);
            drawn += 1;
        }
        // `None`, always: every tile is an untextured rect now, so the whole
        // floor is one batch however many kinds of paving it holds.
        counter.quads(None, drawn);
    }
}

/// The chunk-LOD counterpart of `draw_tile_layer`, for the same terrain
/// backdrop + current-frame-on-top drawing order at extreme zoom-out.
#[allow(clippy::too_many_arguments)]
fn draw_tile_lod_layer(
    tile_lod: &[LodCell],
    tile_lod_runs: &[Run],
    camera: &Camera,
    screen_center: Vec2,
    view_min: Vec2,
    view_max: Vec2,
    registry: &TypeRegistry,
    counter: &mut DrawCallCounter,
) {
    let chunk_px = tiling_quad_size(camera.pixels_per_tile(), LOD_CELL_TILES as f32);
    for run in tile_lod_runs {
        let color = registry.tile_color(run.type_id);
        let mut drawn = 0;
        for cell in &tile_lod[run.range()] {
            let origin = cell.world_origin();
            if origin.x + (LOD_CELL_TILES as f32) < view_min.x
                || origin.x > view_max.x
                || origin.y + (LOD_CELL_TILES as f32) < view_min.y
                || origin.y > view_max.y
            {
                continue;
            }
            let screen = camera.world_to_screen(origin, screen_center);
            draw_lod_cell(screen, chunk_px, color);
            drawn += 1;
        }
        counter.quads(None, drawn);
    }
}

/// Mouse and keyboard input for the active world: panning, zooming,
/// timeline scrubbing, and every playback/display toggle.
/// Applies whatever a left click landed on in the chrome. Returns a surface to
/// switch to, which the caller applies on the next iteration: `worlds[current]`
/// is still mutably borrowed here.
///
/// Separate from `handle_input` despite taking much the same state, because the
/// two answer different questions: this one asks what was clicked, that one
/// asks what the keyboard and the drag are doing.
#[allow(clippy::too_many_arguments)]
fn apply_chrome_click(
    chrome: &Chrome,
    ui: &mut Ui,
    state: &mut ViewerState,
    sequence: &mut FrameSequence,
    camera: &mut Camera,
    follow: &mut FollowState,
    growing_bounds: &[Option<GrowingBounds>],
) -> Option<usize> {
    match chrome.hit(mouse_position().into()) {
        Some(Click::Surface(index)) => {
            state.surfaces_expanded = false;
            return Some(index);
        }
        Some(Click::MoreSurfaces) => state.surfaces_expanded = !state.surfaces_expanded,
        Some(Click::StepBack) => {
            sequence.step_back();
            state.playing = false;
            follow.enabled = false;
        }
        Some(Click::StepForward) => {
            sequence.step_forward();
            state.playing = false;
            follow.enabled = false;
        }
        Some(Click::PlayPause) => {
            state.playing = !state.playing;
            state.play_accum = 0.0;
        }
        // One direction, wrapping at the top. A pill with no second half cannot
        // say which end means slower.
        Some(Click::Speed) => {
            state.play_speed = if state.play_speed >= 8.0 { 0.25 } else { state.play_speed * 2.0 };
        }
        // A one-shot reframe, not the same as `f`: it puts the factory back on
        // screen and leaves the camera alone.
        Some(Click::Fit) => {
            if let Some(bounds) = growing_bounds[sequence.index()] {
                *camera = Camera::fit_framed(
                    bounds.center,
                    bounds.half_extent * 2.0,
                    screen_width(),
                    screen_height(),
                    window_framing(screen_height()),
                );
                follow.enabled = false;
                follow.transition = None;
            }
        }
        Some(Click::Help) => ui.show_keys = true,
        None => {}
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn handle_input(
    camera: &mut Camera,
    sequence: &mut FrameSequence,
    follow: &mut FollowState,
    state: &mut ViewerState,
    timeline: &Timeline,
    screen_center: Vec2,
    jump_targets: &[usize],
    busy: &[usize],
    chrome: &Chrome,
    debug: bool,
) {
    let mouse: Vec2 = mouse_position().into();

    // What a drag does depends on where it started: the scrub bar seeks, a
    // control takes the click, anything else pans. `on_chrome` is latched on
    // press so a drag beginning on a button does not become a pan halfway.
    if is_mouse_button_pressed(MouseButton::Left) {
        state.dragging_timeline = timeline.contains(mouse);
        state.on_chrome = !state.dragging_timeline && chrome.blocks_world(mouse);
    }

    if is_mouse_button_down(MouseButton::Left) && !state.on_chrome {
        if state.dragging_timeline {
            sequence.goto(timeline.index_for_x(mouse.x, sequence.len()));
            state.playing = false;
            follow.enabled = false;
        } else {
            let delta = mouse - state.last_mouse;
            camera.offset -= delta / camera.pixels_per_tile();
            follow.enabled = false;
        }
    }
    state.last_mouse = mouse;

    let (_, wheel_y) = mouse_wheel();
    if wheel_y != 0.0 {
        let before = camera.screen_to_world(mouse, screen_center);
        camera.zoom *= if wheel_y > 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        camera.zoom = camera.zoom.clamp(0.01, 50.0);
        camera.offset = before - (mouse - screen_center) / camera.pixels_per_tile();
        follow.enabled = false;
    }

    if is_key_pressed(KeyCode::Right) || is_key_pressed(KeyCode::Period) {
        sequence.step_forward();
        state.playing = false;
        follow.enabled = false;
    }
    if is_key_pressed(KeyCode::Left) || is_key_pressed(KeyCode::Comma) {
        sequence.step_back();
        state.playing = false;
        follow.enabled = false;
    }
    // Both pairs stop playback and drop auto-follow, a deliberate move being
    // one that should stay put. A jump with nowhere to go does nothing rather
    // than wrapping, which would look like the key jumped at random.
    let jump = |targets: &[usize], sequence: &mut FrameSequence, forward: bool| -> bool {
        let found = if forward {
            crate::viewer::next_mark(targets, sequence.index())
        } else {
            crate::viewer::previous_mark(targets, sequence.index())
        };
        match found {
            Some(frame) => {
                sequence.goto(frame);
                true
            }
            None => false,
        }
    };

    // Letters with shift for reverse rather than brackets or PageUp/Down:
    // bracket keys move between layouts and compact keyboards have no page
    // keys. `m` for mark, `c` for construction.
    let back = is_key_down(KeyCode::LeftShift) || is_key_down(KeyCode::RightShift);
    let mut jumped = false;
    if is_key_pressed(KeyCode::M) {
        jumped |= jump(jump_targets, sequence, !back);
    }
    if is_key_pressed(KeyCode::C) {
        jumped |= jump(busy, sequence, !back);
    }
    if jumped {
        state.playing = false;
        follow.enabled = false;
    }

    if is_key_pressed(KeyCode::Home) {
        sequence.goto(0);
        state.playing = false;
        follow.enabled = false;
    }
    if is_key_pressed(KeyCode::End) {
        sequence.goto(sequence.len() - 1);
        state.playing = false;
        follow.enabled = false;
    }
    if is_key_pressed(KeyCode::Space) {
        state.playing = !state.playing;
        state.play_accum = 0.0;
    }
    // Toggling on clears both, so the next iteration starts a fresh glide
    // from wherever the camera is rather than resuming a stale transition or
    // seeing "no change" and doing nothing.
    if is_key_pressed(KeyCode::F) {
        follow.enabled = !follow.enabled;
        follow.target_bounds = None;
        follow.transition = None;
    }
    // Doubling rather than a linear step, keeping the displayed value a clean
    // power of two.
    if is_key_pressed(KeyCode::Equal) {
        state.play_speed = (state.play_speed * 2.0).min(8.0);
    }
    if is_key_pressed(KeyCode::Minus) {
        state.play_speed = (state.play_speed / 2.0).max(0.25);
    }
    if is_key_pressed(KeyCode::H) {
        state.heatmap_enabled = !state.heatmap_enabled;
    }
    // Written back on change rather than on exit: the viewer is closed by
    // shutting the window, which runs nothing.
    if is_key_pressed(KeyCode::P) {
        state.players_enabled = !state.players_enabled;
        remember_players(state.players_enabled);
    }

    // Renderer A/B tests, not features: either one makes the factory look
    // broken to somebody who pressed the key by accident, so they only answer
    // while the diagnostics they serve are on screen.
    if debug {
        // Same geometry, one flat-rect batch instead of one batch per type,
        // which is what texture binding costs.
        if is_key_pressed(KeyCode::S) {
            state.sprites_enabled = !state.sprites_enabled;
        }
        // Full detail below the chunk threshold, so the per-item CPU cost at
        // extreme zoom-out is directly comparable.
        if is_key_pressed(KeyCode::L) {
            state.lod_enabled = !state.lod_enabled;
        }
    }
}

/// Steps the sequence forward while playing, at `state.play_speed`x real
/// time.
fn advance_playback(sequence: &mut FrameSequence, state: &mut ViewerState) {
    if !state.playing {
        return;
    }
    // A `while`, not `if`: at 8x more than one interval accumulates between
    // two displayed frames, and stepping once per tick would cap playback at
    // the refresh rate.
    state.play_accum += get_frame_time() * state.play_speed;
    while state.play_accum >= PLAY_INTERVAL_SECS {
        state.play_accum -= PLAY_INTERVAL_SECS;
        if sequence.index() + 1 < sequence.len() {
            sequence.step_forward();
        } else {
            state.playing = false;
            state.play_accum = 0.0;
            break;
        }
    }
}

/// How the factory is framed while following it in the window.
///
/// The inset is the greater of the share and what the scrub bar's furniture
/// actually covers. The share is what keeps the window an honest preview of an
/// export; the pixel floor is what still clears the bar on a window short
/// enough that a tenth of it would not.
fn window_framing(screen_height: f32) -> Framing {
    Framing {
        min_size_tiles: AUTO_FOLLOW_MIN_FOCUS_TILES,
        margin: AUTO_FOLLOW_FIT_MARGIN,
        bottom_inset: (screen_height * AUTO_FOLLOW_BOTTOM_INSET).max(timeline_chrome_height()),
    }
}

/// The same, for a frame being exported. No floor: an export draws no scrub
/// bar, so there is no fixed-size furniture to clear, and a pixel count meant
/// for a window would eat a quarter of a small video.
fn export_framing(frame_height: f32) -> Framing {
    Framing {
        min_size_tiles: AUTO_FOLLOW_MIN_FOCUS_TILES,
        margin: AUTO_FOLLOW_FIT_MARGIN,
        bottom_inset: frame_height * AUTO_FOLLOW_BOTTOM_INSET,
    }
}

/// Pixels of the window bottom the scrub bar and its activity graph cover,
/// off the same numbers that draw them. The labels below the bar and the
/// playhead time above the graph are left out: they are small, they are text
/// rather than a solid band, and including them would cost another 40 pixels
/// of factory to clear something nobody is reading the ground through.
fn timeline_chrome_height() -> f32 {
    Timeline::FROM_BOTTOM + ACTIVITY_GAP + ACTIVITY_HEIGHT
}

/// Advances the auto-follow camera transition toward the growing base's
/// current bounds, starting a new glide whenever the tracked area grows
/// past wherever the last one was headed.
fn update_auto_follow(
    camera: &mut Camera,
    follow: &mut FollowState,
    growing_bounds: &[Option<GrowingBounds>],
    sequence_index: usize,
    screen_width: f32,
    screen_height: f32,
) {
    if !follow.enabled {
        return;
    }
    // A new transition starts only once the previous finished and the bounds
    // differ from where it was headed: during active building the area grows
    // almost every frame, so eager retargeting never lands.
    if follow.transition.is_none() {
        if let Some(bounds) = growing_bounds[sequence_index] {
            if follow.target_bounds != Some(bounds) {
                let end = Camera::fit_framed(
                    bounds.center,
                    bounds.half_extent * 2.0,
                    screen_width,
                    screen_height,
                    window_framing(screen_height),
                );
                follow.transition = Some(CameraTransition::new(*camera, end, AUTO_FOLLOW_TRANSITION_SECS));
                follow.target_bounds = Some(bounds);
            }
        }
    }
    if let Some(transition) = &mut follow.transition {
        *camera = transition.step(get_frame_time());
        if transition.is_finished() {
            follow.transition = None;
        }
    }
}

/// Draws the current frame: terrain backdrop, this frame's tiles, then
/// entities, in full detail or chunk LOD depending on zoom.
/// Renders every frame of `world` to a numbered PNG in `request.dir`.
///
/// Draws into an offscreen target, so output size is what was asked for, and
/// otherwise the ordinary draw path, so an export looks like browsing rather
/// than like a second renderer.
async fn export_frames(
    worlds: &mut [WorldView],
    plan: &[(usize, usize)],
    registry: &TypeRegistry,
    sprites: &[Option<Sprite>],
    player_track: &PlayerTrack,
    ui: &Ui,
    request: &ExportRequest,
) -> std::io::Result<usize> {
    // For video the target is a file, so only its parent needs to exist; for
    // a sequence the target is the folder itself.
    let video_path = request.dir.with_extension(if request.mp4 { "mp4" } else { "avi" });
    match request.video {
        true => {
            if let Some(parent) = video_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        false => std::fs::create_dir_all(&request.dir)?,
    }

    // Everything below draws at the oversampled size and only the readback
    // comes down, so the camera, culling and draw code see one surface.
    let ss = request.supersample;
    let (rw, rh) = (request.width * ss, request.height * ss);
    let (w, h) = (rw as f32, rh as f32);
    let mut video = match (request.video, request.mp4) {
        (false, _) => None,
        (true, false) => Some(VideoOut::Avi(AviWriter::create(&video_path, request.width, request.height, request.fps)?)),
        (true, true) => Some(VideoOut::Mp4(Mp4Writer::create(&video_path, request.width, request.height, request.fps)?)),
    };
    let target = render_target(rw, rh);
    target.texture.set_filter(FilterMode::Nearest);

    // Maps the target's pixel space one to one onto screen coordinates, so
    // `draw_world` needs no notion of being offscreen. The negative y is the
    // piece to distrust if output comes out upside down, macroquad flipping y
    // for render targets.
    //
    // `viewport` is not optional despite the type: left unset, macroquad falls
    // back to the window's dimensions and rasterizes a larger export into a
    // window-sized corner.
    let camera = Camera2D {
        render_target: Some(target.clone()),
        zoom: vec2(2.0 / w, -2.0 / h),
        target: vec2(w / 2.0, h / 2.0),
        viewport: Some((0, 0, rw as i32, rh as i32)),
        ..Default::default()
    };
    let screen_center = Vec2::new(w / 2.0, h / 2.0);

    // The whole camera path at once, before a single frame is drawn. An
    // export knows every frame's bounds up front, so the move to a newly
    // reached corner of the map can begin before that corner is built rather
    // than snapping to it after; see `crate::viewer::camera_path`. `fps` is used even
    // for an image sequence, which has no rate of its own: whatever the frames
    // are later assembled at is the rate the pacing was meant for.
    let radius = crate::viewer::smoothing_radius(request.smooth_secs, request.fps);
    // Only for the surfaces this plan actually visits. Smoothing walks every
    // frame of a surface, and a single-surface export of a four planet
    // timelapse would otherwise pay for three it never draws.
    let visited: std::collections::HashSet<usize> = plan.iter().map(|(world, _)| *world).collect();
    let paths: Vec<Option<CameraPath>> = worlds
        .iter()
        .enumerate()
        .map(|(index, world)| {
            visited.contains(&index).then(|| CameraPath::smooth(&world.growing_bounds, world.opening_bounds, radius))
        })
        .collect();

    let total = plan.len();
    let mut counter = DrawCallCounter::new(BATCH_INDEX_CAPACITY);
    let destination = if request.video { video_path.display().to_string() } else { request.dir.display().to_string() };
    let rate = if request.video { format!(" at {} fps", request.fps) } else { String::new() };
    let sampling = if ss > 1 { format!(" ({ss}x supersampled from {rw}x{rh})") } else { String::new() };
    println!("exporting {total} frames at {}x{}{rate}{sampling} to {destination}", request.width, request.height);

    for (step, &(which, index)) in plan.iter().enumerate() {
        let world = &mut worlds[which];
        world.sequence.goto(index);

        // Read off the precomputed path rather than glided through
        // `update_auto_follow`, which advances against wall-clock seconds: an
        // export runs as fast as the disk allows, so its seconds are the
        // finished video's, not this machine's. Fitted here rather than at
        // smoothing time because the path holds resolution-independent boxes
        // and this is where the output size is known. `None` only when there
        // is nothing on this surface at all, and then the opening camera is
        // as good an answer as any.
        if let Some(camera) = paths[which].as_ref().and_then(|path| path.camera_at(index, w, h, export_framing(h))) {
            world.camera = camera;
        }

        set_camera(&camera);
        clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

        let pixels_per_tile = world.camera.pixels_per_tile();
        let frame = world.sequence.current();
        counter.reset();
        draw_world(
            frame,
            world.terrain.as_ref(),
            &world.camera,
            screen_center,
            registry,
            sprites,
            pixels_per_tile > SPRITE_MIN_PIXELS,
            // Items are never aggregated in an export: it has no frame rate to
            // protect, and a cell keeps only its dominant type, which at these
            // zooms is paving swallowing the belts. Sound only because of
            // supersampling.
            //
            // Ground is the opposite. It is most of the drawing and has nothing
            // for a cell to lose, so it follows the same sub-pixel test the
            // interactive view uses. Measured on the supersampled size, so the
            // threshold is stricter here than on screen.
            Detail { items: false, terrain: use_chunk_lod(pixels_per_tile) },
            None,
            &mut counter,
        );
        // Into the render target, so before `set_default_camera` hands drawing
        // back to the window. Sized by `ss` because everything here is drawn
        // oversized and averaged down.
        if request.overlay_players {
            draw_player_markers(ui, player_track, &world.name, frame.tick, &world.camera, screen_center, ss as f32);
        }
        if request.overlay_clock {
            draw_export_clock(ui, frame.tick, h, ss as f32);
        }

        set_default_camera();

        // Read back and write before yielding: the texture is what was just
        // drawn into, and letting the loop advance first would race the next
        // frame's clear.
        let image = downsample(&target.texture.get_texture_data(), ss);
        match &mut video {
            Some(writer) => writer.add(&image)?,
            None => {
                // Numbered by output position, not by the frame's place in its
                // own surface: a following export visits two surfaces and both
                // would otherwise start again at zero and overwrite.
                let path = request.dir.join(format!("frame_{step:05}.png"));
                image.export_png(&path.to_string_lossy());
            }
        }

        if step % 25 == 0 || step + 1 == total {
            print!("\r  {}/{total}", step + 1);
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }
        // Every frame is drawn into the render target and read straight back
        // out, so nothing was ever painted into the window itself: an export
        // sat there black for however long it took. The bar is the same one a
        // load draws, because it is the same question being asked.
        draw_export_progress(step + 1, total, &destination);
        // Hands the frame to the driver. Without it the whole export happens
        // inside one displayed frame and the window sits unresponsive until
        // it finishes.
        next_frame().await;
    }

    if let Some(writer) = video {
        let frames = writer.frames();
        // Writes the index and patches the sizes unknowable until the last
        // frame. Skipping it leaves a file most players refuse outright.
        writer.finish()?;
        let size = std::fs::metadata(&video_path).map(|m| m.len()).unwrap_or(0);
        println!("\ndone: {frames} frames, {:.1} MB, {}", size as f64 / (1024.0 * 1024.0), video_path.display());
    } else {
        println!("\ndone: {} frames in {}", total, request.dir.display());
    }
    Ok(total)
}

/// One frame as JPEG. macroquad hands back RGBA and JPEG has no alpha, so the
/// alpha byte is dropped rather than composited: every pixel came from a
/// clear and opaque draws.
/// Where finished frames go when the export is a video. PNG needs no state of
/// its own and so is not here.
enum VideoOut {
    Avi(AviWriter),
    Mp4(Mp4Writer),
}

impl VideoOut {
    /// MJPEG takes a compressed frame; FFmpeg takes raw pixels, so the MP4
    /// path is encoded once rather than JPEGed and then re-encoded.
    fn add(&mut self, image: &macroquad::texture::Image) -> std::io::Result<()> {
        match self {
            VideoOut::Avi(writer) => writer.add_jpeg(&encode_jpeg(image)?),
            VideoOut::Mp4(writer) => writer.add_frame(&to_rgb24(image)),
        }
    }

    fn frames(&self) -> u32 {
        match self {
            VideoOut::Avi(writer) => writer.frames() as u32,
            VideoOut::Mp4(writer) => writer.frames(),
        }
    }

    fn finish(self) -> std::io::Result<()> {
        match self {
            VideoOut::Avi(writer) => writer.finish(),
            VideoOut::Mp4(writer) => writer.finish(),
        }
    }
}

fn to_rgb24(image: &macroquad::texture::Image) -> Vec<u8> {
    // Rows last to first, a render target reading back bottom-up.
    // `Image::export_png` undoes that itself, so the PNG path never needed it.
    // See `avi.rs`, whose header declares these rows top-down: the two agree,
    // and so does the raw stream `mp4.rs` pipes to ffmpeg.
    let (width, height) = (image.width as usize, image.height as usize);
    let mut rgb: Vec<u8> = Vec::with_capacity(width * height * 3);
    for y in (0..height).rev() {
        let row = &image.bytes[y * width * 4..(y + 1) * width * 4];
        rgb.extend(row.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
    }
    rgb
}

fn encode_jpeg(image: &macroquad::texture::Image) -> std::io::Result<Vec<u8>> {
    let rgb = to_rgb24(image);
    let mut out = Vec::new();
    // 85 rather than the usual default: flat colour with hard edges is what
    // low-quality JPEG smears into halos, and this content compresses well.
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut std::io::Cursor::new(&mut out), 85)
        .encode(&rgb, image.width as u32, image.height as u32, image::ColorType::Rgb8)
        .map_err(std::io::Error::other)?;
    Ok(out)
}

/// Which layers may be collapsed into chunk cells this frame. Two answers
/// rather than one because the reason to keep full detail applies to only one
/// of them.
#[derive(Clone, Copy)]
struct Detail {
    /// Entities and placed floor. Off in an export: a cell keeps only its
    /// dominant type, so a paved area would swallow every belt running through
    /// it, and an export has no frame rate to protect.
    items: bool,
    /// Natural ground. On whenever a tile is already sub-pixel, export
    /// included, ground having no fine structure for a cell to lose.
    terrain: bool,
}

#[allow(clippy::too_many_arguments)]
fn draw_world(
    frame: &RenderFrame,
    terrain: Option<&RenderFrame>,
    camera: &Camera,
    screen_center: Vec2,
    registry: &TypeRegistry,
    sprites: &[Option<Sprite>],
    use_sprites: bool,
    detail: Detail,
    heat: Option<(&[Vec<HeatCell>], u32, usize)>,
    counter: &mut DrawCallCounter,
) {
    // Applied between the ground and the buildings in both branches below,
    // so the factory always draws on top of it at full brightness.
    let paint_heat = |camera: &Camera| {
        if let Some((cells, peak, index)) = heat {
            draw_construction_heat(cells, peak, index, camera, screen_center);
        }
    };
    let (view_min, view_max) = view_bounds(camera, screen_center);

    // Scenery is culled against the ground as well as the screen, so trees
    // stop where the grass does. They cannot agree at capture time: scenery is
    // recorded while playing and ground scanned later, from two boxes at two
    // moments, one cut short by ungenerated chunks. On a real capture scenery
    // overhung ground by 33 tiles a side.
    let scenery_bounds = terrain.and_then(|t| t.tile_bounds).map(|(tmin, tmax)| {
        (Vec2::new(view_min.x.max(tmin.x), view_min.y.max(tmin.y)), Vec2::new(view_max.x.min(tmax.x), view_max.y.min(tmax.y)))
    });
    let bounds_for = |type_id| match scenery_bounds {
        Some(b) if registry.is_terrain_scatter(type_id) => b,
        _ => (view_min, view_max),
    };

    // Natural ground, on its own terms and before everything else.
    //
    // Aggregated whenever a tile is already sub-pixel, an export included,
    // which is where it matters: on a real megabase the ground was 19.7M of a
    // frame's 24M quads, 82% of the drawing, and disabling aggregation for it
    // cost two seconds a frame. A cell keeps only its dominant type, which is
    // why items keep full detail in an export, but ground has no fine
    // structure to lose: four tiles of grass are grass. The visible cost is
    // stair-stepping where two ground types meet, at a scale supersampling is
    // already averaging away.
    if let Some(terrain) = terrain {
        // Scenery rides in the terrain file's entity section. Same detail
        // switch as its tiles, and for the same reason: an ore field across a
        // megabase is millions of quads of background.
        match detail.terrain {
            true => {
                draw_tile_lod_layer(
                    &terrain.tile_lod,
                    &terrain.tile_lod_runs,
                    camera,
                    screen_center,
                    view_min,
                    view_max,
                    registry,
                    counter,
                );
                draw_entity_lod_layer(
                    &terrain.entity_lod,
                    &terrain.entity_lod_runs,
                    camera,
                    screen_center,
                    registry,
                    &bounds_for,
                    counter,
                );
            }
            false => {
                draw_tile_layer(&terrain.tiles, &terrain.tile_runs, camera, screen_center, view_min, view_max, registry, counter);
                draw_entity_layer(
                    &terrain.entities,
                    &terrain.entity_runs,
                    camera,
                    screen_center,
                    registry,
                    sprites,
                    use_sprites,
                    &bounds_for,
                    counter,
                );
            }
        }
    }

    if detail.items {
        // Below LOD_MAX_TILE_PIXELS an item is already sub-pixel, so
        // collapsing a chunk to one quad loses nothing and turns millions of
        // per-item costs into thousands. Precomputed at load.
        draw_tile_lod_layer(&frame.tile_lod, &frame.tile_lod_runs, camera, screen_center, view_min, view_max, registry, counter);
        paint_heat(camera);

        draw_entity_lod_layer(&frame.entity_lod, &frame.entity_lod_runs, camera, screen_center, registry, &bounds_for, counter);
    } else {
        // This frame's floor, then buildings, matching how paving over grass
        // looks in game. Iterating runs keeps the batch intact, sprite and
        // colour being decided once per type.
        draw_tile_layer(&frame.tiles, &frame.tile_runs, camera, screen_center, view_min, view_max, registry, counter);

        paint_heat(camera);

        // Ore before anything standing on it, for the same reason floor goes
        // down before buildings.
        //
        // Stated here because nothing else states it: runs arrive in type
        // order, which is intern order, which is whichever name a capture
        // mentioned first. With both present on the tile (see `Surface::under`)
        // that showed as ore flickering over the factory. Two filtered passes
        // rather than a sorted copy, this loop allocating nothing per frame.
        draw_entity_layer(
            &frame.entities,
            &frame.entity_runs,
            camera,
            screen_center,
            registry,
            sprites,
            use_sprites,
            &bounds_for,
            counter,
        );
    }
}

/// Entities, run by run. Shared with the terrain layer's scenery, so a tree
/// looks the same whichever file it arrived in.
#[allow(clippy::too_many_arguments)]
fn draw_entity_layer(
    entities: &[RenderEntity],
    entity_runs: &[Run],
    camera: &Camera,
    screen_center: Vec2,
    registry: &TypeRegistry,
    sprites: &[Option<Sprite>],
    use_sprites: bool,
    bounds_for: &impl Fn(TypeId) -> (Vec2, Vec2),
    counter: &mut DrawCallCounter,
) {
    let pixels_per_tile = camera.pixels_per_tile();
    // Ore before anything standing on it, for the same reason floor goes
    // down before buildings.
    //
    // Stated here because nothing else states it: runs arrive in type
    // order, which is intern order, which is whichever name a capture
    // mentioned first. With both present on the tile (see `Surface::under`)
    // that showed as ore flickering over the factory. Two filtered passes
    // rather than a sorted copy, this loop allocating nothing per frame.
    let ore = entity_runs.iter().filter(|run| registry.is_resource(run.type_id));
    let built = entity_runs.iter().filter(|run| !registry.is_resource(run.type_id));
    for run in ore.chain(built) {
        let sprite = if use_sprites { sprites[run.type_id as usize].as_ref() } else { None };
        let color = registry.entity_color(run.type_id);
        let rotation_allowed = registry.is_rotation_allowed(run.type_id);
        // Per run rather than per entity: which prototype this is fixes
        // whether it is track, and only the facing varies below.
        let track = registry.is_rail_track(run.type_id);
        let (min, max) = bounds_for(run.type_id);
        let mut drawn = 0;
        for entity in &entities[run.range()] {
            let (w, h) = (entity.w as u32, entity.h as u32);
            let segment = track.then(|| registry.rail_segment(run.type_id, entity.d)).flatten();
            // A rail reaches well past the tile it is recorded on, up to
            // half of a half diagonal's four tiles, so culling it on its
            // 1x1 footprint would drop track that is still on screen.
            let half = match segment {
                Some(segment) => Vec2::splat(segment.length / 2.0),
                None => entity_cull_half_extents(w, h, entity.d, rotation_allowed),
            };
            if entity.x + half.x < min.x || entity.x - half.x > max.x || entity.y + half.y < min.y || entity.y - half.y > max.y {
                continue;
            }
            let screen = camera.world_to_screen(Vec2::new(entity.x, entity.y), screen_center);
            // Track is drawn as a coloured segment, so it takes the plain
            // rectangle path even where an icon did load for it.
            let sprite = segment.is_none().then_some(sprite).flatten();
            let art = match (segment, sprite) {
                (Some(segment), _) => EntityArt::rail(segment),
                (None, Some(sprite)) => entity_source(sprite, entity, rotation_allowed),
                (None, None) => EntityArt::plain(Rect::default(), entity_rotation_radians(w, h, entity.d, rotation_allowed)),
            };
            // A frame that knows its own size in tiles is drawn at that
            // size; everything else still fills its footprint.
            let size = match art.tiles {
                Some(tiles) => tiles * pixels_per_tile,
                None => entity_footprint_size(pixels_per_tile, w, h),
            };
            draw_entity(screen + art.offset * pixels_per_tile, size, color, sprite, &art);
            drawn += 1;
        }
        counter.quads(sprite.map(|_| run.type_id), drawn);
    }
}

/// The chunk-LOD counterpart of `draw_entity_layer`, one flat quad per cell.
#[allow(clippy::too_many_arguments)]
fn draw_entity_lod_layer(
    entity_lod: &[LodCell],
    entity_lod_runs: &[Run],
    camera: &Camera,
    screen_center: Vec2,
    registry: &TypeRegistry,
    bounds_for: &impl Fn(TypeId) -> (Vec2, Vec2),
    counter: &mut DrawCallCounter,
) {
    let chunk_px = tiling_quad_size(camera.pixels_per_tile(), LOD_CELL_TILES as f32);
    for run in entity_lod_runs {
        let color = registry.entity_color(run.type_id);
        let (min, max) = bounds_for(run.type_id);
        let mut drawn = 0;
        for cell in &entity_lod[run.range()] {
            let origin = cell.world_origin();
            if origin.x + (LOD_CELL_TILES as f32) < min.x
                || origin.x > max.x
                || origin.y + (LOD_CELL_TILES as f32) < min.y
                || origin.y > max.y
            {
                continue;
            }
            let screen = camera.world_to_screen(origin, screen_center);
            draw_lod_cell(screen, chunk_px, color);
            drawn += 1;
        }
        counter.quads(None, drawn);
    }
}

/// The renderer diagnostics, behind `F3`. This was the default view once, and
/// is unchanged apart from no longer being on: `zoom 1.42x` and a draw-call
/// budget answer a question only the author asks.
#[allow(clippy::too_many_arguments)]
fn draw_debug_overlay(
    world_name: &str,
    current: usize,
    world_count: usize,
    sequence: &FrameSequence,
    frame: &RenderFrame,
    terrain_tiles: usize,
    camera: &Camera,
    follow_enabled: bool,
    use_lod: bool,
    use_sprites: bool,
    counter: &DrawCallCounter,
) {
    // `{} tiles` stays this frame's own placed floor, with the terrain
    // backdrop called out separately rather than folded in.
    let terrain_suffix = if terrain_tiles > 0 { format!("  |  +{terrain_tiles} terrain tiles") } else { String::new() };
    let total_items = frame.entities.len() + frame.tiles.len() + terrain_tiles;
    // Each line reports the height it used, so the next one stacks under it
    // wherever it ended up: a line that had to wrap pushes the rest down
    // rather than being drawn over.
    let mut hud_y = 20.0;
    hud_y += draw_hud_line(
        &format!(
            "[{} {}/{}]  frame {}/{}  |  {} entities  |  {} tiles{terrain_suffix}  |  zoom {:.2}x  |  follow {}  |  {} fps  |  {:.1} ms",
            world_name,
            current + 1,
            world_count,
            sequence.index() + 1,
            sequence.len(),
            frame.count,
            frame.tiles.len(),
            camera.zoom,
            if follow_enabled { "on" } else { "off" },
            get_fps(),
            get_frame_time() * 1000.0,
        ),
        hud_y,
        HUD_TEXT_SIZE,
        WHITE,
    );

    // Draw calls against quads submitted, and what culling threw away. "of
    // {total}" is specific to full detail: in LOD mode `quads` is cells, and
    // comparing it to an item count would read as "everything culled".
    let detail_text = if use_lod {
        format!(
            "{} chunk cells drawn  |  {} runs  |  LOD on ({total_items} items collapsed)",
            counter.quads,
            frame.entity_lod_runs.len() + frame.tile_lod_runs.len(),
        )
    } else {
        format!(
            "{} quads drawn of {} ({} culled)  |  {} runs  |  sprites {}  |  LOD off",
            counter.quads,
            total_items,
            total_items.saturating_sub(counter.quads),
            frame.entity_runs.len() + frame.tile_runs.len(),
            if use_sprites { "on" } else { "off" },
        )
    };
    draw_hud_line(
        &format!("{} draw calls  |  {detail_text}", counter.calls),
        hud_y + 2.0,
        HUD_TEXT_SIZE,
        Color::new(0.65, 0.92, 1.0, 1.0),
    );
}

/// Draws text with a dark backing so it stays legible over whatever is behind
/// it: the HUD paints onto the world, dark ground in one place and a white
/// platform in the next, so no single colour works and raising alpha does not
/// help. Cardinal offsets rather than a full outline, invisible at these sizes
/// and half the draw calls.
fn draw_text_legible(text: &str, x: f32, y: f32, size: f32, color: Color) {
    let shadow = Color::new(0.0, 0.0, 0.0, 0.85);
    draw_text(text, x + 1.0, y + 1.0, size, shadow);
    draw_text(text, x - 1.0, y + 1.0, size, shadow);
    draw_text(text, x, y, size, color);
}

/// Smallest the HUD may shrink to before wrapping instead. Close to full size
/// on purpose: set low, a 1920 wide window squeezed the controls line to 15px
/// to keep it on one line, and two readable lines beat one nobody can read.
const HUD_MIN_TEXT_SIZE: f32 = 16.0;

/// Left margin the HUD is drawn at, and the gap left on the right so text
/// never runs to the window edge.
const HUD_MARGIN: f32 = 10.0;

/// Draws one HUD line so it fits the window, returning the height used so the
/// caller can stack the next under it. Shrinking is tried first, then wrapping
/// on the `|` the HUD already separates fields with, so a break never lands
/// mid-phrase.
fn draw_hud_line(text: &str, y: f32, size: f32, color: Color) -> f32 {
    let available = (screen_width() - HUD_MARGIN * 2.0).max(1.0);
    let width_at = |s: f32| measure_text(text, None, s as u16, 1.0).width;

    let full = width_at(size);
    if full <= available {
        draw_text_legible(text, HUD_MARGIN, y, size, color);
        return size + 2.0;
    }

    // Shrink, but never below the floor.
    let fitted = (size * available / full).max(HUD_MIN_TEXT_SIZE);
    if width_at(fitted) <= available {
        draw_text_legible(text, HUD_MARGIN, y, fitted, color);
        return fitted + 2.0;
    }

    // Still too wide at the floor: wrap on the field separator.
    let mut used = 0.0;
    let mut line = String::new();
    let flush = |line: &mut String, used: &mut f32| {
        if !line.is_empty() {
            draw_text_legible(line, HUD_MARGIN, y + *used, HUD_MIN_TEXT_SIZE, color);
            *used += HUD_MIN_TEXT_SIZE + 2.0;
            line.clear();
        }
    };
    for field in text.split('|') {
        let field = field.trim();
        let candidate = if line.is_empty() { field.to_string() } else { format!("{line}  |  {field}") };
        if measure_text(&candidate, None, HUD_MIN_TEXT_SIZE as u16, 1.0).width > available && !line.is_empty() {
            flush(&mut line, &mut used);
            line.push_str(field);
        } else {
            line = candidate;
        }
    }
    flush(&mut line, &mut used);
    used
}

/// Text size for the bar's own labels, a step below the HUD's 20.0: these
/// are reference marks read at a glance beside the bar, not primary readouts.
const TIMELINE_LABEL_SIZE: f32 = 16.0;

/// How many frames back the heatmap reaches, oldest fading out. Short on
/// purpose: the glow should trail the construction front rather than
/// accumulate into a map of everywhere you have been.
const HEAT_WINDOW_FRAMES: usize = 10;
/// Alpha at the hottest core. The overlay is opt-in and draws beneath the
/// entities, so it can afford to be bright.
const HEAT_MAX_ALPHA: f32 = 0.85;
/// How far heat bleeds outward from where something was built, in cells. This
/// is what turns scattered lit machines into one glow. See
/// `crate::viewer::recent_heat`.
const HEAT_SPREAD_CELLS: i32 = 3;

// Vertical layout above the scrub bar, stacked upward: graph on the track,
// time label clearing the graph, tooltip clearing the label. Derived from each
// other, all of them moving when the graph's height changes.

/// How tall the activity graph stands at its busiest frame.
const ACTIVITY_HEIGHT: f32 = 26.0;
/// Gap between the track and the graph's baseline, so the two read as
/// separate things rather than the graph growing out of the bar itself.
const ACTIVITY_GAP: f32 = 5.0;
/// Baseline of the current-time label, clearing the graph's full height.
const PLAYHEAD_LABEL_OFFSET: f32 = ACTIVITY_GAP + ACTIVITY_HEIGHT + 14.0;
/// Bottom edge of the hover tooltip, clearing the label above the graph.
const HOVER_TOOLTIP_OFFSET: f32 = PLAYHEAD_LABEL_OFFSET + 10.0;

/// Elapsed game time at `index`, or empty for an index the sequence lacks.
/// Frames carry the real `game.tick`, so this is the capture's own clock.
fn frame_time_label(sequence: &FrameSequence, index: usize) -> String {
    sequence.tick_at(index).map(format_game_time).unwrap_or_default()
}

/// The construction heatmap: where building happened over the last
/// `HEAT_WINDOW_FRAMES`, oldest faintest. Drawn between the ground and the
/// entities, so the factory renders on top at full brightness. Accumulated per
/// rendered frame rather than precomputed, the window sliding.
fn draw_construction_heat(heat: &[Vec<HeatCell>], peak: u32, index: usize, camera: &Camera, screen_center: Vec2) {
    let size = HEAT_CELL_TILES as f32;

    // Culled to the screen before spreading, grown by the spread radius so
    // a hot cell just off the edge still bleeds correctly into view.
    let (view_min, view_max) = view_bounds(camera, screen_center);
    let to_cell = |world: f32| (world / size).floor() as i32;
    let view = (
        to_cell(view_min.x) - HEAT_SPREAD_CELLS,
        to_cell(view_min.y) - HEAT_SPREAD_CELLS,
        to_cell(view_max.x) + HEAT_SPREAD_CELLS,
        to_cell(view_max.y) + HEAT_SPREAD_CELLS,
    );

    let pixels = size * camera.pixels_per_tile();
    for (cx, cy, intensity) in recent_heat(heat, index, HEAT_WINDOW_FRAMES, HEAT_SPREAD_CELLS, peak, Some(view)) {
        let world = Vec2::new(cx as f32 * size, cy as f32 * size);
        let screen = camera.world_to_screen(world, screen_center);
        draw_rectangle(screen.x, screen.y, pixels, pixels, heat_color(intensity));
    }
}

/// Fire: a red core through orange and yellow, fading out at the edges. Hue and
/// alpha both move with intensity, which is what makes edges disappear rather
/// than ending on a yellow rim.
fn heat_color(intensity: f32) -> Color {
    let t = intensity.clamp(0.0, 1.0);
    Color::new(1.0, 0.85 - 0.70 * t, 0.25 - 0.20 * t, t * HEAT_MAX_ALPHA)
}

/// The scrub bar: a filled track to the current frame, tick marks when few
/// enough to read, a playhead, and elapsed time at the ends and playhead.
///
/// Hover stays alive while a drag is in progress even once the pointer has left
/// the bar, dragging pulling it off vertically almost at once.
/// A bookmark's mark: a thin upright tick above the track, deliberately unlike
/// the milestone diamonds below it.
fn draw_bookmark_markers(timeline: &Timeline, sequence: &FrameSequence, bookmark_frames: &[usize]) {
    for &frame in bookmark_frames {
        let x = timeline.x_for_index(frame, sequence.len());
        // A warm yellow, distinct from every milestone color and from the
        // activity graph behind it.
        draw_line(x, timeline.y - 9.0, x, timeline.y - 2.0, 2.0, Color::new(1.0, 0.84, 0.35, 1.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_timeline_bar(
    ui: &Ui,
    timeline: &Timeline,
    sequence: &FrameSequence,
    activity: &[f32],
    milestones: &[Milestone],
    bookmark_frames: &[usize],
    mouse: Vec2,
    scrubbing: bool,
) {
    draw_activity_graph(timeline, sequence, activity);
    draw_bookmark_markers(timeline, sequence, bookmark_frames);

    // Track, filled up to the current frame, a tick per frame when
    // there are few enough to read, and a playhead on top.
    draw_line(timeline.left, timeline.y, timeline.left + timeline.width, timeline.y, 4.0, Color::new(1.0, 1.0, 1.0, 0.25));
    let playhead_x = timeline.x_for_index(sequence.index(), sequence.len());
    draw_line(timeline.left, timeline.y, playhead_x, timeline.y, 4.0, Color::new(1.0, 1.0, 1.0, 0.8));
    if sequence.len() <= 100 {
        for i in 0..sequence.len() {
            let x = timeline.x_for_index(i, sequence.len());
            draw_line(x, timeline.y - 4.0, x, timeline.y + 4.0, 2.0, Color::new(1.0, 1.0, 1.0, 0.4));
        }
    }
    draw_circle(playhead_x, timeline.y, 7.0, WHITE);

    draw_timeline_endpoint_labels(ui, timeline, sequence);
    draw_timeline_playhead_label(ui, timeline, sequence, playhead_x);

    // A hovered marker replaces the frame readout rather than stacking: both
    // want the slot above the bar, and the label carries the time anyway.
    let on_milestone = draw_milestone_markers(ui, timeline, sequence, milestones, mouse);
    if !on_milestone && (timeline.contains(mouse) || scrubbing) {
        draw_timeline_hover(ui, timeline, sequence, mouse);
    }
}

/// How much got built over the run, as a filled area standing on the scrub bar.
///
/// One bar per screen column rather than per frame, which is what makes it read
/// the same at any capture length. Each column takes the loudest frame it
/// covers rather than the mean, the same reason a waveform shows peaks.
fn draw_activity_graph(timeline: &Timeline, sequence: &FrameSequence, activity: &[f32]) {
    if activity.is_empty() || sequence.len() <= 1 {
        return;
    }

    let baseline = timeline.y - ACTIVITY_GAP;
    let playhead_x = timeline.x_for_index(sequence.index(), sequence.len());
    let past = Color::new(1.0, 1.0, 1.0, 0.45);
    let future = Color::new(1.0, 1.0, 1.0, 0.18);

    let columns = timeline.width.max(1.0) as usize;
    for column in 0..columns {
        let x = timeline.left + column as f32;
        // The frames this column covers, from the same index mapping the
        // playhead and click path use. Clamped against `activity` rather than
        // trusted to match: this indexes a slice every column of every frame.
        let last = activity.len() - 1;
        let from = timeline.index_for_x(x, sequence.len()).min(last);
        let to = timeline.index_for_x(x + 1.0, sequence.len()).clamp(from, last);
        let peak = activity[from..=to].iter().copied().fold(0.0f32, f32::max);
        if peak <= 0.0 {
            continue;
        }
        let height = peak * ACTIVITY_HEIGHT;
        let color = if x <= playhead_x { past } else { future };
        draw_line(x, baseline, x, baseline - height, 1.0, color);
    }
}

/// How close the cursor must get to a milestone marker before its label
/// appears. Generous relative to the marker, which is a few pixels wide.
/// HUD text size: a larger glyph carries `draw_text_legible`'s shadow better.
const HUD_TEXT_SIZE: f32 = 21.0;

const MILESTONE_HOVER_SLOP: f32 = 9.0;

/// Marker geometry, in pixels below the track. Sized against
/// `draw_timeline_endpoint_labels`, whose text starts 12px above its +22
/// baseline: the diamond has to clear +10 or it collides with the end times.
const MILESTONE_MARKER_Y: f32 = 6.0;
const MILESTONE_MARKER_RADIUS: f32 = 3.0;

/// The colour a milestone reads as, by kind. Science borrows Factorio's own
/// pack colours, since that association is already in a player's head, so a
/// row of them reads as progress rather than as identical pins.
fn milestone_color(milestone: &Milestone) -> Color {
    let rgb = |r: u8, g: u8, b: u8| Color::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0);
    match &milestone.kind {
        Kind::Science => match milestone.id.as_str() {
            "automation-science-pack" => rgb(220, 60, 50),
            "logistic-science-pack" => rgb(90, 200, 80),
            "military-science-pack" => rgb(120, 130, 140),
            "chemical-science-pack" => rgb(80, 170, 230),
            "production-science-pack" => rgb(190, 90, 220),
            "utility-science-pack" => rgb(230, 200, 70),
            "space-science-pack" => rgb(235, 235, 235),
            "metallurgic-science-pack" => rgb(220, 120, 50),
            "electromagnetic-science-pack" => rgb(70, 110, 220),
            "agricultural-science-pack" => rgb(140, 200, 60),
            "cryogenic-science-pack" => rgb(120, 220, 220),
            "promethium-science-pack" => rgb(200, 60, 140),
            // A modded pack this build has never heard of still gets a
            // stable color of its own, rather than defaulting into one of
            // the vanilla ones and reading as the wrong pack.
            other => color_for(other, 0.65, 0.9),
        },
        Kind::Rocket => rgb(255, 255, 255),
        Kind::Planet => rgb(255, 165, 60),
        Kind::Other(_) => rgb(180, 180, 180),
    }
}

/// Every frame `[` and `]` should stop at, and separately the frames bookmarks
/// sit on. One jump list, "the next interesting thing" being one gesture.
/// Resolved from ticks here, the same tick landing on a different frame
/// depending on how coarsely the timelapse was built.
fn marks_for(milestones: &[Milestone], bookmarks: &[u64], frame_ticks: &[u64]) -> (Vec<usize>, Vec<usize>) {
    let bookmark_frames = crate::viewer::frames_for_ticks(bookmarks, frame_ticks);
    let ticks: Vec<u64> = milestones.iter().map(|m| m.tick).chain(bookmarks.iter().copied()).collect();
    (crate::viewer::frames_for_ticks(&ticks, frame_ticks), bookmark_frames)
}

/// Milestone markers: a small diamond under the bar at each notable moment,
/// labelled on hover. Placed by frame index rather than by interpolating the
/// tick, so a marker sits where clicking takes you. Below the bar, the graph,
/// label and tooltip already stacking upward.
fn draw_milestone_markers(ui: &Ui, timeline: &Timeline, sequence: &FrameSequence, milestones: &[Milestone], mouse: Vec2) -> bool {
    // A sequence is never empty (see `FrameSequence`), so only the milestone
    // list needs guarding.
    if milestones.is_empty() {
        return false;
    }

    // Tucked into the gap between the track and the endpoint labels. The band
    // is a few pixels tall, so the diamond is sized to fit: larger, a marker
    // near either end overlapped the times.
    let marker_y = timeline.y + MILESTONE_MARKER_Y;
    let mut hovered: Option<(&Milestone, f32)> = None;

    for milestone in milestones {
        let index = frame_index_for_tick(sequence, milestone.tick);
        let x = timeline.x_for_index(index, sequence.len());
        let color = milestone_color(milestone);

        // A diamond, drawn as two triangles: distinct at a glance from the
        // bar's own square frame ticks and from the round playhead.
        let r = MILESTONE_MARKER_RADIUS;
        draw_triangle(vec2(x, marker_y - r), vec2(x - r, marker_y), vec2(x + r, marker_y), color);
        draw_triangle(vec2(x, marker_y + r), vec2(x - r, marker_y), vec2(x + r, marker_y), color);

        if (mouse.x - x).abs() <= MILESTONE_HOVER_SLOP && (mouse.y - marker_y).abs() <= MILESTONE_HOVER_SLOP {
            // Nearest wins, so overlapping markers resolve to one label
            // rather than painting several on top of each other.
            let distance = (mouse.x - x).abs();
            if hovered.is_none_or(|(_, best)| distance < best) {
                hovered = Some((milestone, distance));
            }
        }
    }

    let Some((milestone, _)) = hovered else { return false };

    let index = frame_index_for_tick(sequence, milestone.tick);
    let x = timeline.x_for_index(index, sequence.len());
    let label = format!("{}  ({})", milestone.label(), format_game_time(milestone.tick));
    let width = ui.width(&label, TIMELINE_LABEL_SIZE);

    // Above the bar, in the frame readout's slot. Below the marker would be
    // the obvious place, but the bar sits close to the window's bottom edge
    // and the box clipped off screen.
    let padding = 6.0;
    let box_width = width + padding * 2.0;
    let box_height = TIMELINE_LABEL_SIZE + padding * 2.0;
    let box_left = Timeline::tooltip_left(x, box_width, screen_width());
    let box_top = timeline.y - HOVER_TOOLTIP_OFFSET - box_height;

    draw_rectangle(box_left, box_top, box_width, box_height, Color::new(0.0, 0.0, 0.0, 0.9));
    draw_rectangle_lines(box_left, box_top, box_width, box_height, 2.0, milestone_color(milestone));
    ui.text_legible(&label, box_left + padding, box_top + padding + TIMELINE_LABEL_SIZE - 3.0, TIMELINE_LABEL_SIZE, WHITE);

    // A line from the box down to the marker it belongs to, since the two
    // are now on opposite sides of the bar.
    draw_line(x, box_top + box_height, x, marker_y - 5.0, 1.0, Color::new(1.0, 1.0, 1.0, 0.25));
    true
}

/// The frame a tick belongs to: the last at or before it, so a milestone lands
/// on the frame that was showing when it happened. Clamped, a capture being
/// able to start after or stop before a milestone the log records.
fn frame_index_for_tick(sequence: &FrameSequence, tick: u64) -> usize {
    let mut index = 0;
    for i in 0..sequence.len() {
        match sequence.tick_at(i) {
            Some(t) if t <= tick => index = i,
            _ => break,
        }
    }
    index
}

/// Where the capture starts and ends, anchored under the bar's two ends.
/// These bound everything else on the bar: without them the playhead's time
/// is a number with nothing to be a fraction of.
fn draw_timeline_endpoint_labels(ui: &Ui, timeline: &Timeline, sequence: &FrameSequence) {
    let dim = Color::new(1.0, 1.0, 1.0, 0.9);
    let baseline = timeline.y + 24.0;

    let start = frame_time_label(sequence, 0);
    ui.text_legible(&start, timeline.left, baseline, TIMELINE_LABEL_SIZE, dim);

    // Right-aligned so it ends flush with the bar rather than starting at
    // it and overhanging into the window edge as the label grows.
    let end = frame_time_label(sequence, sequence.len().saturating_sub(1));
    let end_width = ui.width(&end, TIMELINE_LABEL_SIZE);
    ui.text_legible(&end, timeline.left + timeline.width - end_width, baseline, TIMELINE_LABEL_SIZE, dim);
}

/// The current frame's time, centered over the playhead and clamped the same
/// way the hover tooltip is, since the playhead reaches the same bar ends
/// the cursor does.
fn draw_timeline_playhead_label(ui: &Ui, timeline: &Timeline, sequence: &FrameSequence, playhead_x: f32) {
    let label = format_game_time(sequence.current().tick);
    let width = ui.width(&label, TIMELINE_LABEL_SIZE);
    let left = Timeline::tooltip_left(playhead_x, width, screen_width());
    ui.text_legible(&label, left, timeline.y - PLAYHEAD_LABEL_OFFSET, TIMELINE_LABEL_SIZE, WHITE);
}

/// A guide line and a boxed readout at the hovered position, so the bar answers
/// "what is here" before committing to a seek. Reports the frame the cursor
/// would land on, via the same `index_for_x` the click path uses: the bar snaps
/// to whole frames.
fn draw_timeline_hover(ui: &Ui, timeline: &Timeline, sequence: &FrameSequence, mouse: Vec2) {
    let index = timeline.index_for_x(mouse.x, sequence.len());
    let hover_x = timeline.x_for_index(index, sequence.len());

    draw_line(hover_x, timeline.y - 10.0, hover_x, timeline.y + 10.0, 2.0, Color::new(1.0, 1.0, 1.0, 0.7));

    let time = frame_time_label(sequence, index);
    let counter = format!("frame {}/{}", index + 1, sequence.len());
    let time_width = ui.width(&time, TIMELINE_LABEL_SIZE);
    let counter_width = ui.width(&counter, TIMELINE_LABEL_SIZE);

    let padding = 8.0;
    let box_width = time_width.max(counter_width) + padding * 2.0;
    let box_height = TIMELINE_LABEL_SIZE * 2.0 + padding * 2.0 + 4.0;
    let box_left = Timeline::tooltip_left(hover_x, box_width, screen_width());
    let box_top = timeline.y - HOVER_TOOLTIP_OFFSET - box_height;

    draw_rectangle(box_left, box_top, box_width, box_height, Color::new(0.0, 0.0, 0.0, 0.9));
    draw_rectangle_lines(box_left, box_top, box_width, box_height, 2.0, Color::new(1.0, 1.0, 1.0, 0.5));
    ui.text_legible(&time, box_left + padding, box_top + padding + TIMELINE_LABEL_SIZE, TIMELINE_LABEL_SIZE, WHITE);
    ui.text_legible(
        &counter,
        box_left + padding,
        box_top + padding + TIMELINE_LABEL_SIZE * 2.0 + 4.0,
        TIMELINE_LABEL_SIZE,
        Color::new(1.0, 1.0, 1.0, 0.85),
    );
}

/// Where the player(s) were as of `tick`, on whichever world/surface is
/// active, looked up fresh each draw rather than cached on the frame,
/// since it's a cheap scan over a tiny sample count (see PlayerTrack).
/// Saves the player-marker preference, ignoring failure. Not being able to
/// remember it costs one keypress next time, which is no reason to interrupt
/// somebody watching a timelapse.
fn remember_players(enabled: bool) {
    let mut settings = crate::settings::Settings::load();
    settings.show_players = Some(enabled);
    let _ = settings.save();
}

/// `scale` is 1 on screen and the supersample factor in an export, where
/// everything is drawn oversized and averaged down. Without it a marker sized
/// in screen pixels comes out half that in a 2x export.
fn draw_player_markers(
    ui: &Ui,
    player_track: &PlayerTrack,
    world_name: &str,
    tick: u64,
    camera: &Camera,
    screen_center: Vec2,
    scale: f32,
) {
    for (name, x, y) in player_track.positions_at(world_name, tick) {
        let screen = camera.world_to_screen(Vec2::new(x, y), screen_center);
        let color = color_for(name, 0.7, 0.95);
        draw_circle(screen.x, screen.y, 9.0 * scale, Color::new(0.0, 0.0, 0.0, 0.6));
        draw_circle(screen.x, screen.y, 6.0 * scale, color);
        ui.text_legible(name, screen.x + 12.0 * scale, screen.y + 4.0 * scale, 18.0 * scale, WHITE);
    }
}

/// The in-game clock burned into an exported frame, bottom left. Off unless
/// asked for: an export is the world alone, and anything over it is a choice.
fn draw_export_clock(ui: &Ui, tick: u64, height: f32, scale: f32) {
    let text = format_game_time(tick);
    let size = 28.0 * scale;
    ui.text_legible(&text, 24.0 * scale, height - 24.0 * scale, size, WHITE);
}

/// Everything the draw loop needs, assembled once before it starts.
struct Loaded {
    registry: TypeRegistry,
    worlds: Vec<WorldView>,
    sprites: Vec<Option<Sprite>>,
    player_track: PlayerTrack,
    milestones: Vec<crate::milestone::Milestone>,
    /// Where bookmarks are written back to, or `None` for a synthetic load
    /// with no directory to keep them in.
    frames_dir: Option<std::path::PathBuf>,
}

/// Reads a capture and turns it into what the loop reads: the registry, one
/// [`WorldView`] per surface, the sprite table, and the player track.
///
/// Split from `main` because the two halves share almost nothing: this ends
/// once `worlds` exists, and the loop below never touches a path or a sidecar
/// again.
async fn load_everything(args: &Args) -> Loaded {
    let mut registry = TypeRegistry::new();
    // Before loading anything: colours resolve when a name is first interned.
    // Missing is the normal state of any capture older than the feature.
    if let Some(dir) = args.path.as_deref().map(std::path::Path::new) {
        if let Some(prototypes) = crate::prototypes::read(dir) {
            println!(
                "using this game's own description of {} tiles and {} entities, {} of them typed",
                prototypes.tiles.len(),
                prototypes.entities.len(),
                prototypes.types.len()
            );
            registry.set_prototypes(prototypes);
        }
    }
    let loaded = load_frames(args, &mut registry).await;

    // Absent entirely (an older capture, or nobody was connected during
    // capture) is normal, not an error: no markers drawn, nothing else
    // affected.
    let players = args
        .path
        .as_deref()
        .map(|p| std::path::Path::new(p).join("players.jsonl"))
        .filter(|p| p.exists())
        .and_then(|p| crate::player_log::read_jsonl(&p).ok())
        .unwrap_or_default();
    let player_track = PlayerTrack::new(players);

    // Alongside the frames, same as the player log: milestones belong to a
    // live capture, so a from-saves timelapse simply has no file and gets an
    // empty list rather than an error.
    let milestones = args
        .path
        .as_deref()
        .map(|p| std::path::Path::new(p).join("milestones.jsonl"))
        .and_then(|p| crate::milestone::read(&p).ok())
        .unwrap_or_default();
    if !milestones.is_empty() {
        println!("{} milestone(s) loaded", milestones.len());
    }

    // Bookmarks live beside the frames, like every other sidecar. `None` for
    // a synthetic load, which has no directory to keep them in.
    let frames_dir: Option<std::path::PathBuf> = args.path.as_deref().map(std::path::PathBuf::from).filter(|p| p.is_dir());

    let data_dir = args.factorio.clone().or_else(locate_factorio).and_then(|exe| install_data_dir(&exe));
    match &data_dir {
        Some(dir) => println!("factorio data: {}", dir.display()),
        None => println!("no factorio install found (pass --factorio); sprites unavailable, using colored shapes"),
    }
    // Written beside the frames by the game that recorded them, and the only
    // source that can answer for a modded prototype. Absent for a timelapse
    // built before icons were dumped, which falls back to the install.
    let dumped_icons = frames_dir.as_ref().map(|dir| dir.join("icons")).filter(|dir| dir.is_dir());
    if let Some(dir) = &dumped_icons {
        let count = std::fs::read_dir(dir).map(|entries| entries.count()).unwrap_or(0);
        println!("icons from this capture's own game: {count}");
    }
    let sprites = load_sprites(dumped_icons.as_deref(), data_dir.as_deref(), &registry).await;
    let with_sprites = sprites.iter().filter(|s| s.is_some()).count();
    println!("{} of {} entity/tile types have sprites", with_sprites, registry.len());

    // One camera per world: panning vulcanus and then tabbing to nauvis with
    // vulcanus's view applied would be disorienting.
    let worlds: Vec<WorldView> = loaded
        .into_iter()
        .map(|(name, sequence, terrain)| {
            // The box first, the camera from it: the walk behind it is over
            // every entity of every frame, and an export wants the box too.
            let opening = Camera::sequence_bounds(&sequence, terrain.as_ref());
            let camera = Camera::from_sequence_bounds(opening, screen_width(), screen_height());
            let opening_bounds = opening.map(|(min, max)| GrowingBounds::from_min_max(min, max));
            let growing_bounds = growing_bounds_per_frame(&sequence, &registry);
            let measured = analyze_activity(&sequence, &registry);
            let activity = activity_heights(&measured.counts);
            let (heat, heat_peak) = (measured.cells, measured.peak_cell);
            // On by default: the fully-zoomed-out whole-sequence fit looks
            // exactly like broken auto-follow unless follow is already active
            // to pull it in to how small the base actually starts.
            let follow = FollowState { enabled: true, ..Default::default() };

            let busy = crate::viewer::busy_stretches(&measured.counts);
            // Milestones and bookmarks share one list, being one gesture.
            // Busy stretches stay separate: derived rather than chosen, and
            // numerous enough to bury the moments somebody marked.
            let frame_ticks: Vec<u64> = (0..sequence.len()).filter_map(|i| sequence.tick_at(i)).collect();
            let bookmarks = frames_dir.as_deref().map(crate::viewer::read_bookmarks).unwrap_or_default();
            let (jump_targets, bookmark_frames) = marks_for(&milestones, &bookmarks, &frame_ticks);

            WorldView {
                name,
                sequence,
                camera,
                growing_bounds,
                opening_bounds,
                activity,
                heat,
                heat_peak,
                jump_targets,
                busy,
                bookmark_frames,
                bookmarks,
                follow,
                terrain,
            }
        })
        .collect();

    Loaded { registry, worlds, sprites, player_track, milestones, frames_dir }
}

/// Renders the surfaces `request` asks for and returns, exporting being a
/// one-shot job rather than a mode of the browser.
///
/// `Err` is a bad `--surface`, which is worth naming before any work starts;
/// a failed render of one surface is reported and the rest still run.
async fn run_export(
    worlds: &mut [WorldView],
    registry: &TypeRegistry,
    sprites: &[Option<Sprite>],
    player_track: &PlayerTrack,
    ui: &Ui,
    request: &ExportRequest,
) -> Result<(), String> {
    let available: Vec<String> = worlds.iter().map(|w| w.name.clone()).collect();

    // One video that goes where the player went, rather than one video per
    // planet each running the full length with nothing on it for the hours
    // nobody was there.
    if request.surface.as_deref().is_some_and(|name| name.eq_ignore_ascii_case("follow")) {
        return run_follow_export(worlds, registry, sprites, player_track, ui, request).await;
    }

    let chosen: Vec<usize> = match request.surface.as_deref() {
        // The default is the busiest surface, which `group_by_surface` already
        // orders first, so the common single-surface case needs no flag.
        None => vec![0],
        Some(name) if name.eq_ignore_ascii_case("all") => (0..worlds.len()).collect(),
        Some(name) => match available.iter().position(|s| s.eq_ignore_ascii_case(name)) {
            Some(index) => vec![index],
            // Naming what is there rather than only what is not: the answer is
            // always in the timelapse the user just pointed at.
            None => {
                return Err(format!(
                    "no surface called \"{name}\". This timelapse has: {}, or \"all\", or \"follow\"",
                    available.join(", ")
                ))
            }
        },
    };

    // One subfolder per surface only when exporting more than one, so a single
    // surface export puts its frames where it was told rather than a level
    // deeper than expected.
    let per_surface = chosen.len() > 1;
    for index in chosen {
        let name = available[index].clone();
        let dir = if per_surface { request.dir.join(&name) } else { request.dir.clone() };
        println!("\nsurface {name}");
        let this = ExportRequest {
            dir,
            width: request.width,
            height: request.height,
            surface: None,
            fps: request.fps,
            video: request.video,
            mp4: request.mp4,
            overlay_players: request.overlay_players,
            overlay_clock: request.overlay_clock,
            supersample: request.supersample,
            smooth_secs: request.smooth_secs,
        };
        let plan: Vec<(usize, usize)> = (0..worlds[index].sequence.len()).map(|frame| (index, frame)).collect();
        if let Err(e) = export_frames(worlds, &plan, registry, sprites, player_track, ui, &this).await {
            eprintln!("export of {name} failed: {e}");
        }
    }
    Ok(())
}

/// One video following the player between surfaces.
///
/// The plan is built before anything is drawn, which is what lets the move
/// list be printed up front: an export is long, and being told it will spend
/// its middle third on Vulcanus beforehand beats discovering it afterwards.
async fn run_follow_export(
    worlds: &mut [WorldView],
    registry: &TypeRegistry,
    sprites: &[Option<Sprite>],
    player_track: &PlayerTrack,
    ui: &Ui,
    request: &ExportRequest,
) -> Result<(), String> {
    let names: Vec<String> = worlds.iter().map(|w| w.name.clone()).collect();
    let per_surface: Vec<&[u64]> = worlds.iter().map(|w| w.sequence.ticks()).collect();
    let ticks = crate::viewer::follow::shared_ticks(&per_surface);

    // Index 0 is the busiest surface, which is where a recording that has not
    // said otherwise should open.
    let schedule = crate::viewer::follow::schedule(&ticks, &names, player_track, 0);

    // Each moment paired with that surface's own frame for it. A surface is
    // only written when something changed on it, so the shared clock asks for
    // moments most of them have no frame of their own at.
    let plan: Vec<(usize, usize)> = ticks
        .iter()
        .zip(&schedule)
        .map(|(&tick, &which)| (which, crate::viewer::follow::frame_at(worlds[which].sequence.ticks(), tick)))
        .collect();

    match player_track.followed() {
        Some(name) => println!("\nfollowing {name} across {} surfaces", names.len()),
        // Worth saying plainly rather than quietly producing one surface: a
        // recording with no player log cannot be followed, and the result is
        // indistinguishable from a normal export unless somebody says so.
        None => println!("\nno player was recorded, so there is nobody to follow: exporting {} throughout", names[0]),
    }
    for (at, which) in crate::viewer::follow::moves(&schedule) {
        println!("  {} from frame {at}", names[which]);
    }

    export_frames(worlds, &plan, registry, sprites, player_track, ui, request)
        .await
        .map(|_| ())
        .map_err(|e| format!("follow export failed: {e}"))
}

/// The viewer, as a screen rather than a program: this used to be `main` in a
/// separate binary, and the two merged so there is one executable to ship.
///
/// Still reached by this program launching itself (see `open_viewer` in
/// `main.rs`), because macroquad allows one window per process and the menu
/// has to stay usable while a timelapse is open.
pub async fn run(args: &[String]) {
    let args = parse_args(args);
    let Loaded { registry, mut worlds, sprites, player_track, milestones, frames_dir } = load_everything(&args).await;
    let mut current = 0usize;

    let mut state = ViewerState {
        last_mouse: mouse_position().into(),
        playing: false,
        play_accum: 0.0,
        play_speed: 1.0,
        dragging_timeline: false,
        on_chrome: false,
        sprites_enabled: true,
        lod_enabled: true,
        heatmap_enabled: false,
        // Shown unless the viewer has been told otherwise: a capture that has
        // player positions recorded them on purpose.
        players_enabled: crate::settings::Settings::load().show_players.unwrap_or(true),
        surfaces_expanded: false,
    };
    let mut counter = DrawCallCounter::new(BATCH_INDEX_CAPACITY);

    // Nothing loaded means the draw loop would index an empty vec and panic
    // with a message that says nothing. Every rejection has already been
    // printed, so this only names the directory. Reachable through ordinary
    // use: captures from an older format version are all rejected.
    if worlds.is_empty() {
        eprintln!(
            "no loadable frames found. Every file that looked like a frame was rejected \
             for the reasons above, which usually means they were written by a different \
             version of this tool than the one reading them."
        );
        return;
    }

    if let Some(request) = &args.export {
        // The export path never builds the interactive chrome, so this is the
        // only thing it needs from it: a font for whatever it burns in.
        let ui = Ui::new();
        if let Err(message) = run_export(&mut worlds, &registry, &sprites, &player_track, &ui, request).await {
            eprintln!("{message}");
        }
        return;
    }

    // Surface names never change, so the chrome reads them from here rather
    // than from `worlds`, which spends every frame mutably borrowed by the
    // destructure below.
    let surface_names: Vec<String> = worlds.iter().map(|w| w.name.clone()).collect();

    let mut ui = Ui::new();
    if !ui.has_font() {
        println!("no UI font found, falling back to the built-in one");
    }
    // Opened once so a first-time viewer is told what the window does, then
    // never again. A `?` in the corner only helps somebody who already
    // suspects there is something to find.
    ui.show_keys = crate::viewer::first_run();

    // A click on a surface chip lands while `worlds[current]` is still
    // mutably borrowed, so it is recorded here and applied at the top of the
    // next iteration. One frame of delay, which is 16ms and invisible.
    let mut pending_surface: Option<usize> = None;

    loop {
        // Captured before the mutable borrow below, which holds `worlds`
        // borrowed for the rest of the loop body.
        let world_count = worlds.len();
        if let Some(index) = pending_surface.take() {
            current = index;
        }
        if is_key_pressed(KeyCode::Tab) && world_count > 1 {
            current = (current + 1) % world_count;
        }
        let WorldView {
            name: world_name,
            sequence,
            camera,
            growing_bounds,
            // Only the export smooths its way out of the opening box. The
            // interactive camera starts there and is pulled in by auto-follow.
            opening_bounds: _,
            activity,
            heat,
            heat_peak,
            jump_targets,
            busy,
            bookmark_frames,
            bookmarks,
            follow,
            terrain,
        } = &mut worlds[current];

        let screen_center = Vec2::new(screen_width() / 2.0, screen_height() / 2.0);
        let timeline = Timeline::for_screen(screen_width(), screen_height());

        // Laid out before input is read, a click having to be tested against
        // the rects actually on screen. Afterwards would test this frame's
        // click against last frame's buttons.
        let clock = frame_time_label(sequence, sequence.index());
        let chrome_state = ChromeState {
            surfaces: &surface_names,
            active: current,
            playing: state.playing,
            play_speed: state.play_speed,
            clock: &clock,
            // What somebody built, not what the frame holds: trees, ore and
            // nests are kept for context and outnumber the factory.
            buildings: sequence.current().building_count(&registry),
            surfaces_expanded: state.surfaces_expanded,
        };
        let chrome = Chrome::layout(&ui, &timeline, &chrome_state);

        if is_key_pressed(KeyCode::F3) {
            ui.show_debug = !ui.show_debug;
        }
        // `?` is shift+/ on a US layout and somewhere else entirely on others,
        // so the slash key is accepted unshifted too. Escape closes but never
        // opens, which is what makes it safe to hit blindly.
        if is_key_pressed(KeyCode::Slash) {
            ui.show_keys = !ui.show_keys;
        }
        if is_key_pressed(KeyCode::Escape) {
            ui.show_keys = false;
            state.surfaces_expanded = false;
        }

        if is_mouse_button_pressed(MouseButton::Left) {
            // Tested before the chrome underneath it: the panel is modal, and
            // a click meant to close it must not also press what it covers.
            if ui.show_keys {
                ui.show_keys = false;
            } else {
                pending_surface = apply_chrome_click(&chrome, &mut ui, &mut state, sequence, camera, follow, growing_bounds);
            }
        }

        // Everything below is suppressed while the panel is up, so keys that
        // would otherwise scrub or pan behind it do nothing until it is
        // dismissed. The panel is the only modal thing in the viewer.
        if !ui.show_keys {
            handle_input(
                camera,
                sequence,
                follow,
                &mut state,
                &timeline,
                screen_center,
                jump_targets,
                busy,
                &chrome,
                ui.show_debug,
            );
        } else {
            // Still tracked while the panel is up, so dismissing it and
            // dragging does not snap the camera by however far the cursor
            // moved in between.
            state.last_mouse = mouse_position().into();
        }

        // Stored as that frame's tick, and written straight away rather than
        // on exit, the viewer being a window somebody closes.
        if is_key_pressed(KeyCode::B) {
            if let (Some(dir), Some(tick)) = (frames_dir.as_deref(), sequence.tick_at(sequence.index())) {
                match bookmarks.iter().position(|&t| t == tick) {
                    Some(at) => {
                        bookmarks.remove(at);
                    }
                    None => bookmarks.push(tick),
                }
                bookmarks.sort_unstable();
                crate::viewer::write_bookmarks(dir, bookmarks);
                let frame_ticks: Vec<u64> = (0..sequence.len()).filter_map(|i| sequence.tick_at(i)).collect();
                (*jump_targets, *bookmark_frames) = marks_for(&milestones, bookmarks, &frame_ticks);
            }
        }
        advance_playback(sequence, &mut state);
        update_auto_follow(camera, follow, growing_bounds, sequence.index(), screen_width(), screen_height());

        clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

        let pixels_per_tile = camera.pixels_per_tile();
        let use_sprites = state.sprites_enabled && pixels_per_tile > SPRITE_MIN_PIXELS;
        let use_lod = state.lod_enabled && use_chunk_lod(pixels_per_tile);
        let frame = sequence.current();
        counter.reset();

        let heat_layer = state.heatmap_enabled.then(|| (heat.as_slice(), *heat_peak, sequence.index()));
        draw_world(
            frame,
            terrain.as_ref(),
            camera,
            screen_center,
            &registry,
            &sprites,
            use_sprites,
            Detail { items: use_lod, terrain: use_lod },
            heat_layer,
            &mut counter,
        );

        if state.players_enabled {
            draw_player_markers(&ui, &player_track, world_name, sequence.current().tick, camera, screen_center, 1.0);
        }
        draw_timeline_bar(
            &ui,
            &timeline,
            sequence,
            activity,
            &milestones,
            bookmark_frames,
            mouse_position().into(),
            state.dragging_timeline,
        );
        chrome.draw(&ui, &chrome_state);

        // Both overlays go last so nothing can be drawn over them, and the
        // key panel last of all: it is the only modal thing here, and while
        // it is up it should cover the diagnostics too.
        if ui.show_debug {
            let terrain_tiles = terrain.as_ref().map_or(0, |t| t.tiles.len());
            draw_debug_overlay(
                world_name,
                current,
                world_count,
                sequence,
                frame,
                terrain_tiles,
                camera,
                follow.enabled,
                use_lod,
                use_sprites,
                &counter,
            );
        }
        if ui.show_keys {
            draw_key_panel(&ui);
        }

        next_frame().await;
    }
}
