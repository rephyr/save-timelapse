//! Thin macroquad glue: argument parsing, the window loop, input polling,
//! and drawing. Everything else (camera math, color hashing, the type
//! registry, frame grouping, the draw-call model, synthetic data, the frame
//! sequence, sprite path resolution) lives in `lib.rs`, where it's unit
//! tested, since none of it can be once it touches macroquad's window/input
//! globals or does real texture loading.

use std::path::PathBuf;
use std::time::Instant;

use macroquad::prelude::*;
use save_timelapse::export::install_data_dir;
use save_timelapse::locate::locate_factorio;
use save_timelapse::milestone::{Kind, Milestone};
use viewer::{
    activity_heights, analyze_activity, color_for, entity_cull_half_extents, entity_footprint_size,
    entity_rotation_radians, format_game_time, growing_bounds_per_frame, icon_path, icon_source_rect,
    is_rotation_allowed, recent_heat, synthetic_frame, synthetic_tiles, use_chunk_lod, Camera, CameraTransition,
    DrawCallCounter, FrameSequence, GrowingBounds, HeatCell, LoadProgress, LodCell, PlayerTrack, ProgressBar,
    RenderFrame, RenderTile, Run, Timeline, TypeRegistry, HEAT_CELL_TILES, LOD_CELL_TILES,
};

const ZOOM_STEP: f32 = 1.1;
const PLAY_INTERVAL_SECS: f32 = 0.25; // ~4 frames/sec auto-play
/// Floor on how tight auto-follow can zoom in. Low rather than generous: the
/// point is only to stop a single 1x1 entity (the very first thing ever
/// placed, before a second one exists to give the box real size) from
/// filling the screen. By the time a real starter cluster exists (drill,
/// furnace, a belt or two), its own natural extent is already bigger than
/// this and the floor stops mattering. A high floor here was fighting the
/// exact thing auto-follow is for: hugging the base as it actually is,
/// starting from how small it actually starts.
const AUTO_FOLLOW_MIN_FOCUS_TILES: f32 = 6.0;
/// How much smaller than a bare edge-to-edge fit auto-follow zooms: tight,
/// unlike `fit_frames`'s own, more generous margin. The point of following
/// is to hug the actual buildings closely, with the edge of the frame only
/// slightly beyond the furthest one placed.
const AUTO_FOLLOW_FIT_MARGIN: f32 = 0.92;
/// How long, in real seconds, a camera move to the base's newly-grown extent
/// takes, matching TLBE (the most-downloaded Factorio timelapse mod)'s own
/// camera transition model: a fixed-duration linear glide from wherever the
/// camera currently is, started fresh whenever the tracked area grows,
/// rather than an exponential approach. See `Camera::CameraTransition`.
const AUTO_FOLLOW_TRANSITION_SECS: f32 = 1.5;
/// Below this, a sprite is imperceptible and not worth a texture draw over a
/// flat rect: the zoom-based sprites/shapes split agreed back in the
/// milestone-1 discussion.
const SPRITE_MIN_PIXELS: f32 = 12.0;

/// macroquad starts a new GPU draw call whenever its batch buffer fills, so
/// the default capacity (10,000 vertices / 5,000 indices) caps a draw call at
/// 833 quads, meaning even perfectly texture-sorted output costs a draw call
/// per 833 entities. Raising it lifts that ceiling to 4,096 quads.
///
/// Not raised further because indices are `u16` and get offset by the running
/// vertex count (`quad_gl.rs::geometry`), so vertex capacity cannot exceed
/// 65,536 without corrupting geometry, and because macroquad allocates one
/// GPU buffer of this size per draw call it has ever used.
const BATCH_QUAD_CAPACITY: usize = 4096;
const BATCH_VERTEX_CAPACITY: usize = BATCH_QUAD_CAPACITY * 4;
const BATCH_INDEX_CAPACITY: usize = BATCH_QUAD_CAPACITY * 6;

/// Frame files parsed at once before being folded into spans and dropped.
/// Bounds peak load memory at roughly this many frames, while still being
/// wide enough to keep every core busy parsing (see `load_batch`).
const LOAD_BATCH_FRAMES: usize = 16;

/// How often loading pauses to draw the progress bar. Yielding per item would
/// pace loading to the display's refresh rate; this keeps the bar responsive
/// while leaving loading effectively at full speed.
const PROGRESS_REDRAW: std::time::Duration = std::time::Duration::from_millis(33);

fn window_conf() -> macroquad::conf::Conf {
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

/// Everything the draw loop tracks per world/surface: playback position,
/// camera, and auto-follow state. A named struct rather than a growing
/// tuple, since the fields are no longer few enough to keep straight
/// positionally once auto-follow joined them.
struct WorldView {
    name: String,
    sequence: FrameSequence,
    camera: Camera,
    /// The whole base's bounding box as of each frame, monotonically
    /// growing, precomputed once at load. See `viewer::growing_bounds_per_frame`.
    growing_bounds: Vec<Option<GrowingBounds>>,
    /// How much got built in each frame, already normalized to 0..1 for
    /// drawing. Precomputed at load alongside `growing_bounds`, which walks
    /// the same entities: recovering this needs a diff between consecutive
    /// frames (see `viewer::activity_per_frame`), far too much to redo every
    /// time the bar is drawn.
    activity: Vec<f32>,
    /// Where construction happened in each frame, binned into cells for the
    /// heatmap overlay. Same pass as `activity` above.
    heat: Vec<Vec<HeatCell>>,
    /// The busiest single cell of the run, so heat brightness stays anchored
    /// to a fixed reference rather than to whatever is currently on screen.
    heat_peak: u32,
    follow: FollowState,
    /// This surface's natural-terrain layer, loaded once (not per frame:
    /// terrain never changes after the baseline, see
    /// `save_timelapse::world::World::terrain_frame`). `None` when terrain
    /// capture was off, or this capture predates it.
    terrain: Option<RenderFrame>,
}

#[derive(Default)]
struct FollowState {
    enabled: bool,
    /// Whichever bounds the current (or last finished) transition is/was
    /// headed towards, so a new one only starts once the tracked area
    /// actually grows, not on every rendered frame.
    target_bounds: Option<GrowingBounds>,
    /// The in-flight move to `target_bounds`, if any. Deliberately left
    /// running to completion rather than restarted on every frame the
    /// tracked area happens to grow a little further: during active
    /// building, the area can grow on nearly every displayed frame, and
    /// restarting a multi-second glide that often meant it was constantly
    /// interrupted a fraction of the way in, always chasing a target several
    /// steps stale, never actually centered or zoomed to the true current
    /// extent. Waiting for one glide to finish before checking again means
    /// it always catches up fully, straight to wherever the base currently
    /// is, before starting the next one.
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
    sprites_enabled: bool,
    lod_enabled: bool,
    heatmap_enabled: bool,
}

struct Args {
    path: Option<String>,
    synthetic_entities: Option<usize>,
    synthetic_tile_count: Option<usize>,
    factorio: Option<PathBuf>,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut result =
        Args { path: None, synthetic_entities: None, synthetic_tile_count: None, factorio: None };

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
            other => result.path = Some(other.to_string()),
        }
        i += 1;
    }

    result
}

// Loading, with a progress bar

fn draw_loading(progress: &LoadProgress) {
    clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

    let bar = ProgressBar::centered(screen_width(), screen_height());
    draw_rectangle(bar.left, bar.top, bar.width, bar.height, Color::new(1.0, 1.0, 1.0, 0.12));
    draw_rectangle(
        bar.left,
        bar.top,
        bar.filled_width(progress),
        bar.height,
        Color::new(0.45, 0.75, 1.0, 0.9),
    );
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
/// `--synthetic-tiles` is a knob independent of `--synthetic`/a frame path:
/// it layers a synthetic floor on top of whichever entities were requested,
/// since the real risk case (a fully-paved megabase) is tile-heavy in a way
/// the entity-only stress test doesn't cover. Both stay single-frame:
/// playback is for real exported sequences, not synthetic stress tests.
/// One timeline per surface, named, so the caller can switch between them
/// (tab, in the running viewer) instead of only ever seeing whichever one
/// happened to be busiest.
async fn load_frames(args: &Args, registry: &mut TypeRegistry) -> Vec<(String, FrameSequence, Option<RenderFrame>)> {
    let mut last = Instant::now();
    let mut progress =
        LoadProgress { phase: "reading frames", detail: String::new(), done: 0, total: 0 };

    // Single-frame paths (synthetic, or the default fixture) share the tail
    // of this function; only a real directory can have more than one world.
    let single = if let Some(n) = args.synthetic_entities {
        progress.phase = "building synthetic frame";
        progress.detail = format!("{n} entities");
        redraw_progress(&progress, &mut last, true).await;
        Some(synthetic_frame(n))
    } else if args.path.is_none() {
        let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frames/frame_0004.stfr");
        println!("no frame given, defaulting to {default}");
        progress.detail = "default fixture".to_string();
        redraw_progress(&progress, &mut last, true).await;
        let bytes = std::fs::read(default).expect("failed to read default fixture");
        Some(save_timelapse::frame::read_binary(&bytes).expect("failed to parse frame"))
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
        if let Some(sequence) = builder.finish() {
            result.push((name, sequence, None));
        }
    } else {
        let path = args.path.as_ref().expect("checked above");
        println!("loading {path}");
        let path = std::path::Path::new(path);
        let paths = viewer::frame_paths(path).expect("failed to enumerate frames");

        // A single frame file's terrain (if it even has one) would sit beside
        // it in its parent directory, same as a real capture's.
        let terrain_dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        let terrain_file_paths = viewer::terrain_paths(terrain_dir).unwrap_or_default();

        // Terrain starts loading here, before the frames, since it is a
        // separate set of files with nothing shared until both are done.
        // Two independent waits back to back would cost their sum; started
        // together, the wait is bounded by whichever is slower. It stays on
        // the load-everything-at-once path deliberately: there is at most one
        // terrain file per surface, so it is bounded by surface count rather
        // than by capture length.
        progress.total = paths.len() + terrain_file_paths.len();
        progress.detail =
            format!("{} core(s)", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
        let terrain_load = viewer::ParallelFrameLoad::start(terrain_file_paths);

        // Grouped from headers rather than from parsed frames, which is what
        // makes the streaming below possible: knowing each file's surface and
        // tick is enough to fix the order, and that costs a bounded read per
        // file instead of holding the whole capture in memory to sort it.
        progress.phase = "reading frame headers";
        redraw_progress(&progress, &mut last, true).await;
        let grouped = viewer::group_paths_by_surface(paths);

        progress.phase = "loading frames";
        let mut done = 0usize;
        for (name, paths) in grouped {
            let mut builder = FrameSequence::builder();
            for chunk in paths.chunks(LOAD_BATCH_FRAMES) {
                // One batch is parsed across every core, folded into spans,
                // and dropped before the next is read, so peak memory is a
                // batch plus the spans rather than the whole capture.
                for mut frame in viewer::load_batch(chunk) {
                    if let Some(n) = args.synthetic_tile_count {
                        frame.tiles = synthetic_tiles(n);
                    }
                    builder.push(&RenderFrame::from_frame(frame, registry));
                }
                done += chunk.len();
                progress.done = done + terrain_load.done();
                redraw_progress(&progress, &mut last, false).await;
            }
            if let Some(sequence) = builder.finish() {
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
        let mut terrain_by_surface: std::collections::HashMap<String, save_timelapse::frame::Frame> =
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

/// A loaded icon plus the region of it that's the actual icon. Vanilla and
/// Space Age icon files are a mipmap strip (the full-size icon followed by
/// progressively smaller copies), not a single image, so drawing the whole
/// texture stretched into an entity's box renders every copy squashed
/// together. `icon_rect` crops to just the first (primary) one.
struct Sprite {
    texture: Texture2D,
    icon_rect: Rect,
}

/// Sprites indexed by `TypeId`, so drawing never hashes a name.
///
/// Best-effort: a Factorio install found (given or auto-detected) doesn't
/// mean every icon resolves, since this only covers vanilla/Space-Age
/// naming, not arbitrary mods. Missing icons just mean that type keeps
/// using its colored shape, never an error.
async fn load_sprites(data_dir: Option<&std::path::Path>, registry: &TypeRegistry) -> Vec<Option<Sprite>> {
    let mut sprites: Vec<Option<Sprite>> = (0..registry.len()).map(|_| None).collect();
    let Some(data_dir) = data_dir else {
        return sprites;
    };

    let mut last = Instant::now();
    let mut progress = LoadProgress {
        phase: "loading sprites",
        detail: String::new(),
        done: 0,
        total: registry.len(),
    };

    for (id, name) in registry.names().iter().enumerate() {
        progress.done = id;
        progress.detail = name.clone();
        redraw_progress(&progress, &mut last, false).await;

        if let Some(path) = icon_path(data_dir, name).and_then(|p| p.to_str().map(str::to_owned)) {
            if let Ok(texture) = load_texture(&path).await {
                let icon_rect = icon_source_rect(texture.width(), texture.height());
                sprites[id] = Some(Sprite { texture, icon_rect });
            }
        }
    }

    progress.done = registry.len();
    redraw_progress(&progress, &mut last, true).await;
    sprites
}

// Drawing

fn draw_entity(center: Vec2, size: Vec2, rotation: f32, color: Color, sprite: Option<&Sprite>) {
    let top_left = center - size / 2.0;
    match sprite {
        Some(sprite) => draw_texture_ex(
            &sprite.texture,
            top_left.x,
            top_left.y,
            WHITE,
            DrawTextureParams {
                dest_size: Some(size),
                source: Some(sprite.icon_rect),
                rotation,
                ..Default::default()
            },
        ),
        None => draw_rectangle_ex(
            center.x,
            center.y,
            size.x,
            size.y,
            DrawRectangleParams { rotation, offset: Vec2::splat(0.5), color },
        ),
    }
}

/// Tiles are corner positioned, unlike entities, so `screen` here is the
/// tile's top-left corner rather than its center.
fn draw_tile(screen: Vec2, size: f32, color: Color, sprite: Option<&Sprite>) {
    match sprite {
        Some(sprite) => draw_texture_ex(
            &sprite.texture,
            screen.x,
            screen.y,
            WHITE,
            DrawTextureParams {
                dest_size: Some(Vec2::splat(size)),
                source: Some(sprite.icon_rect),
                ..Default::default()
            },
        ),
        None => draw_rectangle(screen.x, screen.y, size, size, color),
    }
}

/// One level-of-detail cell: always a flat rect, never a sprite. A chunk
/// aggregates several types into "whichever is dominant," so no single icon
/// would be honest about what's actually there.
fn draw_lod_cell(screen: Vec2, size: f32, color: Color) {
    draw_rectangle(screen.x, screen.y, size, size, color);
}

/// World-space view rectangle, so culling is a pair of comparisons per item
/// instead of a full world-to-screen transform per item followed by a screen
/// bounds test. Only survivors pay for the transform.
fn view_bounds(camera: &Camera, screen_center: Vec2) -> (Vec2, Vec2) {
    let min = camera.screen_to_world(Vec2::ZERO, screen_center);
    let max = camera.screen_to_world(Vec2::new(screen_width(), screen_height()), screen_center);
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
    sprites: &[Option<Sprite>],
    use_sprites: bool,
    counter: &mut DrawCallCounter,
) {
    let tile_size = camera.pixels_per_tile().max(1.0);
    for run in tile_runs {
        let sprite = if use_sprites { sprites[run.type_id as usize].as_ref() } else { None };
        let color = registry.tile_color(run.type_id);
        let mut drawn = 0;
        for tile in &tiles[run.range()] {
            // A tile at (x,y) covers [x,x+1) x [y,y+1).
            let (x, y) = (tile.x as f32, tile.y as f32);
            if x + 1.0 < view_min.x || x > view_max.x || y + 1.0 < view_min.y || y > view_max.y {
                continue;
            }
            let screen = camera.world_to_screen(Vec2::new(x, y), screen_center);
            draw_tile(screen, tile_size, color, sprite);
            drawn += 1;
        }
        counter.quads(sprite.map(|_| run.type_id), drawn);
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
    let chunk_px = camera.pixels_per_tile() * LOD_CELL_TILES as f32;
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
fn handle_input(
    camera: &mut Camera,
    sequence: &mut FrameSequence,
    follow: &mut FollowState,
    state: &mut ViewerState,
    timeline: &Timeline,
    screen_center: Vec2,
) {
    let mouse: Vec2 = mouse_position().into();

    // Which a drag does depends on where it started: grabbing the
    // scrub bar seeks, anywhere else pans the camera, same as a video
    // player's scrubber taking priority over the content behind it.
    if is_mouse_button_pressed(MouseButton::Left) {
        state.dragging_timeline = timeline.contains(mouse);
    }

    if is_mouse_button_down(MouseButton::Left) {
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
    // Toggling on clears both `target_bounds` and `transition`, so the
    // very next iteration always starts a brand new glide from wherever
    // the camera currently is (possibly just moved by the manual
    // controls that disengaged follow in the first place) rather than
    // either resuming a stale in-flight transition or, if the current
    // frame's bounds happen to match whatever was last targeted before,
    // seeing "no change" and doing nothing at all.
    if is_key_pressed(KeyCode::F) {
        follow.enabled = !follow.enabled;
        follow.target_bounds = None;
        follow.transition = None;
    }
    // Doubling/halving rather than a linear step: matches how video
    // players commonly expose speed, and keeps the displayed value a
    // clean power of two (0.25x, 0.5x, 1x, 2x, 4x, 8x) instead of an
    // arbitrary decimal.
    if is_key_pressed(KeyCode::Equal) {
        state.play_speed = (state.play_speed * 2.0).min(8.0);
    }
    if is_key_pressed(KeyCode::Minus) {
        state.play_speed = (state.play_speed / 2.0).max(0.25);
    }
    // Sprites off is the A/B for what texture binding costs: same
    // geometry, one flat-rect batch instead of one batch per type.
    if is_key_pressed(KeyCode::S) {
        state.sprites_enabled = !state.sprites_enabled;
    }
    // LOD off is the A/B for what per-item CPU cost costs at extreme
    // zoom-out: forces full-detail rendering even below the chunk
    // threshold, so the difference is directly comparable.
    if is_key_pressed(KeyCode::H) {
        state.heatmap_enabled = !state.heatmap_enabled;
    }

    if is_key_pressed(KeyCode::L) {
        state.lod_enabled = !state.lod_enabled;
    }
}

/// Steps the sequence forward while playing, at `state.play_speed`x real
/// time.
fn advance_playback(sequence: &mut FrameSequence, state: &mut ViewerState) {
    if !state.playing {
        return;
    }
    // A `while`, not `if`: at high speed multipliers more than one
    // interval's worth can accumulate between two frames (e.g. at
    // 8x and a 60fps display, ~8 frames advance in the time one
    // used to), and stepping only once per tick would cap the
    // visible playback rate at the display's refresh rate instead
    // of the requested speed.
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
    // A new transition only starts once any previous one has finished
    // (`follow.transition.is_none()`) *and* the currently displayed
    // frame's bounds differ from wherever that last glide was headed.
    // Checking "is a transition already running" first, not just "did
    // the target change", matters because during active building the
    // tracked area can grow on nearly every displayed frame. Without
    // this, a multi-frame-per-second retarget rate would restart the
    // glide before it ever got there, permanently chasing a target
    // several steps stale rather than ever actually landing on the
    // true current extent. Waiting it out means every glide always
    // finishes fully caught up before the next one begins.
    if follow.transition.is_none() {
        if let Some(bounds) = growing_bounds[sequence_index] {
            if follow.target_bounds != Some(bounds) {
                let end = Camera::fit_bounds(
                    bounds.center,
                    bounds.half_extent * 2.0,
                    screen_width,
                    screen_height,
                    AUTO_FOLLOW_MIN_FOCUS_TILES,
                    AUTO_FOLLOW_FIT_MARGIN,
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

/// Draws the current frame: terrain backdrop (if loaded), then its own
/// tiles, then entities, in either full detail or chunk-LOD depending on
/// zoom.
#[allow(clippy::too_many_arguments)]
fn draw_world(
    frame: &RenderFrame,
    terrain: Option<&RenderFrame>,
    camera: &Camera,
    screen_center: Vec2,
    registry: &TypeRegistry,
    sprites: &[Option<Sprite>],
    use_sprites: bool,
    use_lod: bool,
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
    let pixels_per_tile = camera.pixels_per_tile();
    let (view_min, view_max) = view_bounds(camera, screen_center);

    if use_lod {
        // Below LOD_MAX_TILE_PIXELS a full-detail tile or entity is
        // already sub-pixel, so nothing is lost by collapsing a whole
        // LOD_CELL_TILES-square chunk to one quad, and everything is
        // gained: a chunk grid over the same world area this base
        // actually spans is thousands of quads, not millions, which is
        // the difference between paying a per-item CPU cost 3.4 million
        // times a frame and paying it a few thousand times.
        //
        // Precomputed once at load (`RenderFrame::from_frame`), not
        // here: binning millions of items into chunks is itself too
        // slow to redo every rendered frame, which is exactly the cost
        // this path exists to avoid.
        if let Some(terrain) = terrain {
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
        }
        draw_tile_lod_layer(
            &frame.tile_lod,
            &frame.tile_lod_runs,
            camera,
            screen_center,
            view_min,
            view_max,
            registry,
            counter,
        );
        paint_heat(camera);

        let chunk_px = pixels_per_tile * LOD_CELL_TILES as f32;
        for run in &frame.entity_lod_runs {
            let color = registry.entity_color(run.type_id);
            let mut drawn = 0;
            for cell in &frame.entity_lod[run.range()] {
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
    } else {
        // Terrain backdrop first (if loaded), then this frame's own
        // placed floor on top of it, then buildings on top of that,
        // matching how paving over grass looks in-game.
        //
        // Iterating runs rather than raw items is what keeps the batch
        // intact: the sprite and color are decided once per type, so
        // macroquad sees a long stretch of quads sharing one texture
        // instead of a texture change per item.
        if let Some(terrain) = terrain {
            draw_tile_layer(
                &terrain.tiles,
                &terrain.tile_runs,
                camera,
                screen_center,
                view_min,
                view_max,
                registry,
                sprites,
                use_sprites,
                counter,
            );
        }
        draw_tile_layer(
            &frame.tiles,
            &frame.tile_runs,
            camera,
            screen_center,
            view_min,
            view_max,
            registry,
            sprites,
            use_sprites,
            counter,
        );

        paint_heat(camera);

        for run in &frame.entity_runs {
            let sprite = if use_sprites { sprites[run.type_id as usize].as_ref() } else { None };
            let color = registry.entity_color(run.type_id);
            let rotation_allowed = is_rotation_allowed(registry.name(run.type_id));
            let mut drawn = 0;
            for entity in &frame.entities[run.range()] {
                let (w, h) = (entity.w as u32, entity.h as u32);
                let half = entity_cull_half_extents(w, h, entity.d, rotation_allowed);
                if entity.x + half.x < view_min.x
                    || entity.x - half.x > view_max.x
                    || entity.y + half.y < view_min.y
                    || entity.y - half.y > view_max.y
                {
                    continue;
                }
                let screen = camera.world_to_screen(Vec2::new(entity.x, entity.y), screen_center);
                let size = entity_footprint_size(pixels_per_tile, w, h);
                let rotation = entity_rotation_radians(w, h, entity.d, rotation_allowed);
                draw_entity(screen, size, rotation, color, sprite);
                drawn += 1;
            }
            counter.quads(sprite.map(|_| run.type_id), drawn);
        }
    }
}

/// The three-line text HUD: world/frame/camera/follow status, the
/// draw-call profiling readout, and the control hints.
#[allow(clippy::too_many_arguments)]
fn draw_hud(
    world_name: &str,
    current: usize,
    world_count: usize,
    sequence: &FrameSequence,
    frame: &RenderFrame,
    terrain_tiles: usize,
    camera: &Camera,
    follow_enabled: bool,
    state: &ViewerState,
    use_lod: bool,
    use_sprites: bool,
    counter: &DrawCallCounter,
) {
    // `{} tiles` stays scoped to this frame's own placed floor (unchanged
    // meaning: how much is this frame doing), with the terrain backdrop
    // (loaded once, not per frame) called out separately rather than
    // folded into the same number.
    let terrain_suffix =
        if terrain_tiles > 0 { format!("  |  +{terrain_tiles} terrain tiles") } else { String::new() };
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

    // The profiling readout: draw calls against quads actually submitted,
    // and how much culling threw away. Counts this viewer's own geometry
    // only; macroquad's text rendering adds a few calls of its own.
    //
    // "of {total} ({culled})" only makes sense against full-detail item
    // counts, so it's specific to that branch: in LOD mode `quads` is
    // chunk cells, not items, and comparing it to a millions-large item
    // count would misleadingly read as "everything culled."
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
    hud_y += draw_hud_line(
        &format!("{} draw calls  |  {detail_text}", counter.calls),
        hud_y + 2.0,
        HUD_TEXT_SIZE,
        Color::new(0.65, 0.92, 1.0, 1.0),
    );

    let tab_hint = if world_count > 1 { "  |  tab switches world" } else { "" };
    draw_hud_line(
        &format!(
            "drag to pan, scroll to zoom  |  left/right step, space {}, home/end jump  |  -/= speed ({}x)  |  s toggles sprites, l toggles LOD  |  h build heatmap ({})  |  f auto-follows the growing base ({})  |  drag the bar below to scrub{tab_hint}",
            if state.playing { "pause" } else { "play" },
            state.play_speed,
            if state.heatmap_enabled { "on" } else { "off" },
            if follow_enabled { "on" } else { "off" },
        ),
        hud_y + 4.0,
        HUD_TEXT_SIZE,
        WHITE,
    );
}

/// Draws text with a dark backing so it stays legible over whatever the world
/// happens to be behind it.
///
/// The HUD and the timeline's labels are painted straight onto the rendered
/// world, which is dark ground in one place and a bright concrete pad or a
/// white space platform in the next, so a single text colour cannot be
/// readable everywhere. Raising the alpha alone does not fix it: the problem
/// is that light thin glyphs have no edge against a light background.
///
/// A one pixel offset shadow in near black gives every glyph that edge, which
/// costs a second `draw_text` per label and nothing else. Cardinal offsets
/// rather than a full outline: at these sizes the difference is invisible and
/// this is half the draw calls.
fn draw_text_legible(text: &str, x: f32, y: f32, size: f32, color: Color) {
    let shadow = Color::new(0.0, 0.0, 0.0, 0.85);
    draw_text(text, x + 1.0, y + 1.0, size, shadow);
    draw_text(text, x - 1.0, y + 1.0, size, shadow);
    draw_text(text, x, y, size, color);
}

/// Smallest the HUD is allowed to shrink to before it starts wrapping
/// instead.
///
/// Deliberately close to the full size, so shrinking only ever absorbs a
/// small overflow and anything worse wraps. Set low, a 1920 wide window
/// squeezed the controls line down to 15px to keep it on one line, which is
/// the wrong trade: two lines at a readable size beat one line nobody can
/// read, and the whole reason this exists is that the HUD was hard to read.
const HUD_MIN_TEXT_SIZE: f32 = 16.0;

/// Left margin the HUD is drawn at, and the gap left on the right so text
/// never runs to the window edge.
const HUD_MARGIN: f32 = 10.0;

/// Draws one HUD line so it actually fits the window, and returns the height
/// it used so the caller can stack the next one under it.
///
/// The HUD lines are long, and their length is not a design choice: they name
/// every key binding and every live statistic. At 1920 wide they fit at full
/// size; on a smaller or scaled display they used to run straight off the
/// right edge, taking the last few readouts with them.
///
/// So the size is chosen from the window rather than fixed. Shrinking is
/// tried first, down to `HUD_MIN_TEXT_SIZE`, since one line at 15px reads
/// better than two at 21px. Past that it wraps, splitting on the `|` the HUD
/// already separates its fields with, so a break never lands mid-phrase.
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

/// How many frames back the construction heatmap reaches, oldest fading to
/// nothing. Short on purpose: the point is showing where work is happening
/// *now*, so the glow trails the construction front and dies out behind it
/// rather than accumulating into a map of everywhere you have ever been.
const HEAT_WINDOW_FRAMES: usize = 10;
/// Alpha at the hottest core. The overlay is opt-in (`h`) and draws beneath
/// the entities, so it can afford to be genuinely bright: the factory is
/// painted over the top of it regardless, and anything dimmer read as barely
/// there against the ground.
const HEAT_MAX_ALPHA: f32 = 0.85;
/// How far heat bleeds outward from where something was actually built, in
/// cells (so `HEAT_SPREAD_CELLS * HEAT_CELL_TILES` tiles). This is what turns
/// a scatter of individually lit machines into one glow over the area being
/// worked on. See `viewer::recent_heat`.
const HEAT_SPREAD_CELLS: i32 = 3;

// Vertical layout above the scrub bar, stacked upward from the track: the
// activity graph sits directly on it, the current-time label clears the
// graph, and the hover tooltip clears the label. Named and derived from each
// other rather than written as separate magic offsets, since every one of
// them has to move whenever the graph's height changes.

/// How tall the activity graph stands at its busiest frame.
const ACTIVITY_HEIGHT: f32 = 26.0;
/// Gap between the track and the graph's baseline, so the two read as
/// separate things rather than the graph growing out of the bar itself.
const ACTIVITY_GAP: f32 = 5.0;
/// Baseline of the current-time label, clearing the graph's full height.
const PLAYHEAD_LABEL_OFFSET: f32 = ACTIVITY_GAP + ACTIVITY_HEIGHT + 14.0;
/// Bottom edge of the hover tooltip, clearing the label above the graph.
const HOVER_TOOLTIP_OFFSET: f32 = PLAYHEAD_LABEL_OFFSET + 10.0;

/// Elapsed game time at `index`, or an empty string for an index the
/// sequence does not have. Frames carry the real `game.tick` they were
/// emitted at (see `replay::run`), so this is the capture's own clock rather
/// than anything derived from frame numbering.
fn frame_time_label(sequence: &FrameSequence, index: usize) -> String {
    sequence.tick_at(index).map(format_game_time).unwrap_or_default()
}

/// The construction heatmap: where building happened over the last
/// `HEAT_WINDOW_FRAMES`, as translucent warm quads, oldest faintest.
///
/// Drawn between the ground and the entities (see `draw_world`), which is
/// what keeps it from covering the view in the way that actually matters:
/// the factory itself renders on top at full brightness, so no belt or
/// assembler is ever dimmed or hazed by it. Low alpha alone would not do
/// that, since it would still wash over everything built.
///
/// Accumulated per rendered frame rather than precomputed per frame, because
/// the window slides: cell lists are a few hundred entries each and only
/// `HEAT_WINDOW_FRAMES` of them are ever touched, so this is a few thousand
/// operations, unlike the per-entity pass that produced them.
fn draw_construction_heat(
    heat: &[Vec<HeatCell>],
    peak: u32,
    index: usize,
    camera: &Camera,
    screen_center: Vec2,
) {
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
    for (cx, cy, intensity) in
        recent_heat(heat, index, HEAT_WINDOW_FRAMES, HEAT_SPREAD_CELLS, peak, Some(view))
    {
        let world = Vec2::new(cx as f32 * size, cy as f32 * size);
        let screen = camera.world_to_screen(world, screen_center);
        draw_rectangle(screen.x, screen.y, pixels, pixels, heat_color(intensity));
    }
}

/// Fire: a red core, through orange and yellow as it cools, fading out
/// entirely at the edges.
///
/// Hue and alpha both move with intensity, which is what makes the edges
/// disappear rather than ending on a visible yellow rim. Red is reserved for
/// the hottest cells specifically because the spread above saturates dense
/// construction and leaves isolated machines dim, so red genuinely means
/// "a lot went up right here" rather than just "something is here".
fn heat_color(intensity: f32) -> Color {
    let t = intensity.clamp(0.0, 1.0);
    Color::new(1.0, 0.85 - 0.70 * t, 0.25 - 0.20 * t, t * HEAT_MAX_ALPHA)
}

/// The scrub bar: a filled track up to the current frame, tick marks when
/// there are few enough to read, a playhead circle, and the elapsed game
/// time at the ends and at the playhead.
///
/// `mouse` and `scrubbing` drive the hover readout. Hover is kept alive
/// while a drag is in progress even once the pointer has left the bar's hit
/// box, since dragging a scrubber pulls the pointer off it vertically almost
/// immediately, and losing the readout at exactly that moment would take it
/// away whenever it is being used most deliberately.
fn draw_timeline_bar(
    timeline: &Timeline,
    sequence: &FrameSequence,
    activity: &[f32],
    milestones: &[Milestone],
    mouse: Vec2,
    scrubbing: bool,
) {
    draw_activity_graph(timeline, sequence, activity);

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

    draw_timeline_endpoint_labels(timeline, sequence);
    draw_timeline_playhead_label(timeline, sequence, playhead_x);

    // A hovered marker replaces the frame readout rather than stacking with
    // it: both want the same slot above the bar, and pointing at a milestone
    // is a request to read the milestone. The label carries the time anyway,
    // which is most of what the frame readout would have said.
    let on_milestone = draw_milestone_markers(timeline, sequence, milestones, mouse);
    if !on_milestone && (timeline.contains(mouse) || scrubbing) {
        draw_timeline_hover(timeline, sequence, mouse);
    }
}

/// How much got built over the run, as a filled area standing on the scrub
/// bar: tall where a lot went up, flat where nothing did.
///
/// Drawn as one vertical bar per screen column rather than one per frame,
/// which is what makes it read the same at any capture length. A 40-frame
/// capture would otherwise leave visible gaps between marks, and a
/// 4000-frame one would pile dozens of frames onto each column and draw
/// them all. Each column takes the loudest frame it covers rather than the
/// mean, so a single busy frame stays visible instead of being averaged away
/// by the quiet ones either side of it, the same reason a waveform display
/// shows peaks.
///
/// Everything up to the playhead is drawn brighter, so the graph doubles as
/// the progress fill rather than fighting with it.
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
        // The frames this column covers, taken from the same index mapping
        // the playhead and click path use so the graph lines up with them.
        // Clamped against `activity` rather than trusted to match the
        // sequence length: they are built together and do match, but this
        // indexes a slice every column of every frame, so it should not be
        // one refactor away from a panic in the draw loop.
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

/// How close the cursor has to get to a milestone marker, in pixels, before
/// its label appears. Generous relative to the marker, which is only a few
/// pixels wide: the label is the point of the marker, and hunting for a
/// pixel-perfect hover on a bar you are also dragging is miserable.
/// HUD text size. Nudged up from 20: the readouts sit over the rendered
/// world at whatever brightness it happens to be, and a slightly larger glyph
/// carries the shadow in `draw_text_legible` better than a thin one does.
const HUD_TEXT_SIZE: f32 = 21.0;

const MILESTONE_HOVER_SLOP: f32 = 9.0;

/// Marker geometry, in pixels below the track. Sized against
/// `draw_timeline_endpoint_labels`, whose text starts 12px above its +22
/// baseline: the diamond has to clear +10 or it collides with the end times.
const MILESTONE_MARKER_Y: f32 = 6.0;
const MILESTONE_MARKER_RADIUS: f32 = 3.0;

/// The color a milestone reads as, by kind.
///
/// Science borrows Factorio's own pack colors, since that association is
/// already in a player's head from the lab and the tech tree, and a row of
/// them along the bar then reads as progress through the game rather than as
/// a row of identical pins.
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

/// Milestone markers: a small diamond under the bar at each notable moment,
/// with its label on hover.
///
/// Placed by frame index rather than by interpolating the tick across the
/// bar, so a marker sits exactly where clicking would take you. The bar snaps
/// to whole frames, so a tick-interpolated marker would sit slightly off from
/// the frame it names, which is worst precisely when frames are sparse.
///
/// Below the bar rather than above it: the activity graph, playhead label and
/// hover tooltip already stack upward, and markers want to be near the track
/// they annotate rather than on the far side of a graph.
fn draw_milestone_markers(
    timeline: &Timeline,
    sequence: &FrameSequence,
    milestones: &[Milestone],
    mouse: Vec2,
) -> bool {
    // A sequence is never empty (see `FrameSequence`), so only the milestone
    // list needs guarding.
    if milestones.is_empty() {
        return false;
    }

    // Tucked into the gap between the track and the endpoint time labels
    // below it. The band is only a few pixels tall, so the diamond is sized
    // to fit rather than the other way round: at its first size a marker
    // landing near either end of the bar overlapped "0m" or the end time.
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
    let width = measure_text(&label, None, TIMELINE_LABEL_SIZE as u16, 1.0).width;

    // Above the bar, in the frame readout's slot. Below the marker would be
    // the obvious place, but the bar sits close to the window's bottom edge
    // and there is not room for a box down there: it clipped straight off
    // the screen.
    let padding = 6.0;
    let box_width = width + padding * 2.0;
    let box_height = TIMELINE_LABEL_SIZE + padding * 2.0;
    let box_left = Timeline::tooltip_left(x, box_width, screen_width());
    let box_top = timeline.y - HOVER_TOOLTIP_OFFSET - box_height;

    draw_rectangle(box_left, box_top, box_width, box_height, Color::new(0.0, 0.0, 0.0, 0.9));
    draw_rectangle_lines(box_left, box_top, box_width, box_height, 2.0, milestone_color(milestone));
    draw_text_legible(&label, box_left + padding, box_top + padding + TIMELINE_LABEL_SIZE - 3.0, TIMELINE_LABEL_SIZE, WHITE);

    // A line from the box down to the marker it belongs to, since the two
    // are now on opposite sides of the bar.
    draw_line(x, box_top + box_height, x, marker_y - 5.0, 1.0, Color::new(1.0, 1.0, 1.0, 0.25));
    true
}

/// The frame a tick belongs to: the last one at or before it, so a milestone
/// lands on the frame that was showing when it happened rather than the one
/// after. Clamped to the ends, since a capture can start after or stop before
/// a milestone the log still records.
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
fn draw_timeline_endpoint_labels(timeline: &Timeline, sequence: &FrameSequence) {
    let dim = Color::new(1.0, 1.0, 1.0, 0.9);
    let baseline = timeline.y + 24.0;

    let start = frame_time_label(sequence, 0);
    draw_text_legible(&start, timeline.left, baseline, TIMELINE_LABEL_SIZE, dim);

    // Right-aligned so it ends flush with the bar rather than starting at
    // it and overhanging into the window edge as the label grows.
    let end = frame_time_label(sequence, sequence.len().saturating_sub(1));
    let end_width = measure_text(&end, None, TIMELINE_LABEL_SIZE as u16, 1.0).width;
    draw_text_legible(&end, timeline.left + timeline.width - end_width, baseline, TIMELINE_LABEL_SIZE, dim);
}

/// The current frame's time, centered over the playhead and clamped the same
/// way the hover tooltip is, since the playhead reaches the same bar ends
/// the cursor does.
fn draw_timeline_playhead_label(timeline: &Timeline, sequence: &FrameSequence, playhead_x: f32) {
    let label = format_game_time(sequence.current().tick);
    let width = measure_text(&label, None, TIMELINE_LABEL_SIZE as u16, 1.0).width;
    let left = Timeline::tooltip_left(playhead_x, width, screen_width());
    draw_text_legible(&label, left, timeline.y - PLAYHEAD_LABEL_OFFSET, TIMELINE_LABEL_SIZE, WHITE);
}

/// A guide line at the hovered position plus a boxed readout of the time and
/// frame number there, so the bar answers "what is at this point" before
/// committing to a seek.
///
/// Deliberately reports the frame the cursor would actually land on, via the
/// same `index_for_x` the click path uses, rather than interpolating time
/// across the bar: frames are spaced by a fixed tick interval but the bar
/// snaps to whole frames, so an interpolated label would disagree with what
/// clicking there produces.
fn draw_timeline_hover(timeline: &Timeline, sequence: &FrameSequence, mouse: Vec2) {
    let index = timeline.index_for_x(mouse.x, sequence.len());
    let hover_x = timeline.x_for_index(index, sequence.len());

    draw_line(hover_x, timeline.y - 10.0, hover_x, timeline.y + 10.0, 2.0, Color::new(1.0, 1.0, 1.0, 0.7));

    let time = frame_time_label(sequence, index);
    let counter = format!("frame {}/{}", index + 1, sequence.len());
    let time_width = measure_text(&time, None, TIMELINE_LABEL_SIZE as u16, 1.0).width;
    let counter_width = measure_text(&counter, None, TIMELINE_LABEL_SIZE as u16, 1.0).width;

    let padding = 8.0;
    let box_width = time_width.max(counter_width) + padding * 2.0;
    let box_height = TIMELINE_LABEL_SIZE * 2.0 + padding * 2.0 + 4.0;
    let box_left = Timeline::tooltip_left(hover_x, box_width, screen_width());
    let box_top = timeline.y - HOVER_TOOLTIP_OFFSET - box_height;

    draw_rectangle(box_left, box_top, box_width, box_height, Color::new(0.0, 0.0, 0.0, 0.9));
    draw_rectangle_lines(box_left, box_top, box_width, box_height, 2.0, Color::new(1.0, 1.0, 1.0, 0.5));
    draw_text_legible(&time, box_left + padding, box_top + padding + TIMELINE_LABEL_SIZE, TIMELINE_LABEL_SIZE, WHITE);
    draw_text_legible(
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
fn draw_player_markers(player_track: &PlayerTrack, world_name: &str, tick: u64, camera: &Camera, screen_center: Vec2) {
    for (name, x, y) in player_track.positions_at(world_name, tick) {
        let screen = camera.world_to_screen(Vec2::new(x, y), screen_center);
        let color = color_for(name, 0.7, 0.95);
        draw_circle(screen.x, screen.y, 9.0, Color::new(0.0, 0.0, 0.0, 0.6));
        draw_circle(screen.x, screen.y, 6.0, color);
        draw_text_legible(name, screen.x + 12.0, screen.y + 4.0, 18.0, WHITE);
    }
}

#[macroquad::main(window_conf)]
async fn main() {
    let args = parse_args();

    let mut registry = TypeRegistry::new();
    let loaded = load_frames(&args, &mut registry).await;

    // Absent entirely (an older capture, or nobody was connected during
    // capture) is normal, not an error: no markers drawn, nothing else
    // affected.
    let players = args
        .path
        .as_deref()
        .map(|p| std::path::Path::new(p).join("players.jsonl"))
        .filter(|p| p.exists())
        .and_then(|p| save_timelapse::player_log::read_jsonl(&p).ok())
        .unwrap_or_default();
    let player_track = PlayerTrack::new(players);

    // Alongside the frames, same as the player log: milestones belong to a
    // live capture, so a from-saves timelapse simply has no file and gets an
    // empty list rather than an error.
    let milestones = args
        .path
        .as_deref()
        .map(|p| std::path::Path::new(p).join("milestones.jsonl"))
        .and_then(|p| save_timelapse::milestone::read(&p).ok())
        .unwrap_or_default();
    if !milestones.is_empty() {
        println!("{} milestone(s) loaded", milestones.len());
    }

    let data_dir = args.factorio.or_else(locate_factorio).and_then(|exe| install_data_dir(&exe));
    match &data_dir {
        Some(dir) => println!("factorio data: {}", dir.display()),
        None => println!("no factorio install found (pass --factorio); sprites unavailable, using colored shapes"),
    }
    let sprites = load_sprites(data_dir.as_deref(), &registry).await;
    let with_sprites = sprites.iter().filter(|s| s.is_some()).count();
    println!("{} of {} entity/tile types have sprites", with_sprites, registry.len());

    // One camera per world rather than one shared: panning/zooming vulcanus
    // and then tabbing to nauvis with vulcanus's view still applied would be
    // disorienting, and each world's own frames are what its camera was
    // fitted to in the first place.
    let mut worlds: Vec<WorldView> = loaded
        .into_iter()
        .map(|(name, sequence, terrain)| {
            let camera =
                Camera::fit_sequence(&sequence, terrain.as_ref(), screen_width(), screen_height());
            let growing_bounds = growing_bounds_per_frame(&sequence, &registry);
            let measured = analyze_activity(&sequence, &registry);
            let activity = activity_heights(&measured.counts);
            let (heat, heat_peak) = (measured.cells, measured.peak_cell);
            // On by default: opening straight into the fully-zoomed-out
            // whole-sequence fit (see `Camera::fit_frames` above) looks
            // exactly like broken auto-follow (big from the very first
            // frame, never zooming out further) unless auto-follow is
            // already active to immediately pull it in to how small the
            // base actually starts. `f` still toggles it off for anyone who
            // wants full manual control from the start.
            let follow = FollowState { enabled: true, ..Default::default() };
            WorldView { name, sequence, camera, growing_bounds, activity, heat, heat_peak, follow, terrain }
        })
        .collect();
    let mut current = 0usize;

    let mut state = ViewerState {
        last_mouse: mouse_position().into(),
        playing: false,
        play_accum: 0.0,
        play_speed: 1.0,
        dragging_timeline: false,
        sprites_enabled: true,
        lod_enabled: true,
        heatmap_enabled: false,
    };
    let mut counter = DrawCallCounter::new(BATCH_INDEX_CAPACITY);

    // Nothing loaded means the draw loop below would index `worlds[current]`
    // on an empty vec and panic with "index out of bounds: the len is 0",
    // which says nothing about the actual problem. Every individual reason a
    // frame was rejected has already been printed by now (wrong magic,
    // unsupported version, unreadable), so this only has to name the
    // directory and stop.
    //
    // Reachable through ordinary use, not just a bad argument: pointing the
    // viewer at a directory of captures from an older format version rejects
    // every one of them, and that is exactly what an upgrade leaves behind.
    if worlds.is_empty() {
        eprintln!(
            "no loadable frames found. Every file that looked like a frame was rejected \
             for the reasons above, which usually means they were written by a different \
             version of this tool than the one reading them."
        );
        return;
    }

    loop {
        // Captured before the mutable borrow below, which holds `worlds`
        // borrowed for the rest of the loop body.
        let world_count = worlds.len();
        if is_key_pressed(KeyCode::Tab) && world_count > 1 {
            current = (current + 1) % world_count;
        }
        let WorldView {
            name: world_name, sequence, camera, growing_bounds, activity, heat, heat_peak, follow, terrain
        } =
            &mut worlds[current];

        let screen_center = Vec2::new(screen_width() / 2.0, screen_height() / 2.0);
        let timeline = Timeline::for_screen(screen_width(), screen_height());

        handle_input(camera, sequence, follow, &mut state, &timeline, screen_center);
        advance_playback(sequence, &mut state);
        update_auto_follow(camera, follow, growing_bounds, sequence.index(), screen_width(), screen_height());

        clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

        let pixels_per_tile = camera.pixels_per_tile();
        let use_sprites = state.sprites_enabled && pixels_per_tile > SPRITE_MIN_PIXELS;
        let use_lod = state.lod_enabled && use_chunk_lod(pixels_per_tile);
        let frame = sequence.current();
        counter.reset();

        let heat_layer =
            state.heatmap_enabled.then(|| (heat.as_slice(), *heat_peak, sequence.index()));
        draw_world(
            frame,
            terrain.as_ref(),
            camera,
            screen_center,
            &registry,
            &sprites,
            use_sprites,
            use_lod,
            heat_layer,
            &mut counter,
        );

        let terrain_tiles = terrain.as_ref().map_or(0, |t| t.tiles.len());
        draw_hud(
            world_name,
            current,
            world_count,
            sequence,
            frame,
            terrain_tiles,
            camera,
            follow.enabled,
            &state,
            use_lod,
            use_sprites,
            &counter,
        );
        draw_timeline_bar(
            &timeline,
            sequence,
            activity,
            &milestones,
            mouse_position().into(),
            state.dragging_timeline,
        );
        draw_player_markers(&player_track, world_name, sequence.current().tick, camera, screen_center);

        next_frame().await;
    }
}
