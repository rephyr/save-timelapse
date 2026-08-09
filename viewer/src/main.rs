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
use viewer::{
    color_for, entity_cull_half_extents, entity_footprint_size, entity_rotation_radians, format_game_time,
    growing_bounds_per_frame, icon_path, icon_source_rect, is_rotation_allowed, synthetic_frame, synthetic_tiles,
    use_chunk_lod, Camera, CameraTransition, DrawCallCounter, FrameSequence, GrowingBounds, LoadProgress, LodCell,
    PlayerTrack, ProgressBar, RenderFrame, RenderTile, Run, Timeline, TypeRegistry, LOD_CELL_TILES,
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
    draw_text(&headline, bar.left, bar.top - 14.0, 24.0, WHITE);
    if !progress.detail.is_empty() {
        draw_text(&progress.detail, bar.left, bar.top + bar.height + 26.0, 18.0, Color::new(1.0, 1.0, 1.0, 0.6));
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

    let worlds: Vec<(String, Vec<save_timelapse::frame::Frame>, Option<save_timelapse::frame::Frame>)> =
        if let Some(mut frame) = single {
            if let Some(n) = args.synthetic_tile_count {
                frame.tiles = synthetic_tiles(n);
            }
            // Synthetic/default-fixture loads have no on-disk directory to
            // look for a terrain file in, and nothing produces one for them.
            vec![(frame.surface.clone(), vec![frame], None)]
        } else {
            let path = args.path.as_ref().expect("checked above");
            println!("loading {path}");
            let path = std::path::Path::new(path);
            let paths = viewer::frame_paths(path).expect("failed to enumerate frames");

            // A single frame file's terrain (if it even has one) would sit
            // beside it in its parent directory, same as a real capture's.
            let terrain_dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
            let terrain_file_paths = viewer::terrain_paths(terrain_dir).unwrap_or_default();

            // Reading and parsing each file is independent work, so it happens
            // across every available core rather than one file at a time. On a
            // real megabase capture this is the dominant cost of opening the
            // viewer. Converting to a RenderFrame needs a single, consistently
            // numbered TypeRegistry, so that part stays sequential below.
            //
            // Terrain starts loading here too, not after the regular frames
            // finish: it's a separate set of files with nothing shared until
            // both are done, discovered straight from the directory (see
            // `terrain_paths`) rather than waited on until grouping below
            // learns the surface list from the (unrelated) frame files. Two
            // independent waits back to back would cost their sum; started
            // together, the wait is bounded by whichever is slower.
            progress.total = paths.len() + terrain_file_paths.len();
            progress.detail = format!(
                "{} core(s)",
                std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
            );
            let load = viewer::ParallelFrameLoad::start(paths);
            let terrain_load = viewer::ParallelFrameLoad::start(terrain_file_paths);
            let mut loaded_frames = None;
            let mut loaded_terrain = None;
            let (loaded, loaded_terrain) = loop {
                progress.done = load.done() + terrain_load.done();
                redraw_progress(&progress, &mut last, true).await;
                // `poll` only ever yields its result once, so each is only
                // ever called again while still waiting on that one; the
                // other's own result, once captured, is left alone even
                // while this loop keeps running for whichever is slower.
                loaded_frames = loaded_frames.or_else(|| load.poll());
                loaded_terrain = loaded_terrain.or_else(|| terrain_load.poll());
                if let (Some(_), Some(_)) = (&loaded_frames, &loaded_terrain) {
                    break (loaded_frames.take().unwrap(), loaded_terrain.take().unwrap());
                }
            };
            progress.done = progress.total;
            redraw_progress(&progress, &mut last, true).await;

            let mut terrain_by_surface: std::collections::HashMap<String, save_timelapse::frame::Frame> =
                loaded_terrain.into_iter().map(|frame| (frame.surface.clone(), frame)).collect();

            // Grouped before converting, not after: the mod's raw baseline
            // output writes every surface at the same tick, and grouping is
            // what keeps all of them (one timeline per surface) instead of
            // collapsing to whichever was busiest.
            let mut grouped = viewer::group_by_surface(loaded);
            if let Some(n) = args.synthetic_tile_count {
                for (_, frames) in &mut grouped {
                    for frame in frames {
                        frame.tiles = synthetic_tiles(n);
                    }
                }
            }

            grouped
                .into_iter()
                .map(|(name, frames)| {
                    let terrain = terrain_by_surface.remove(&name);
                    (name, frames, terrain)
                })
                .collect()
        };

    progress.phase = "converting frames";
    progress.done = 0;
    progress.total = worlds.iter().map(|(_, frames, _)| frames.len()).sum();
    let mut result = Vec::with_capacity(worlds.len());
    for (name, frames, terrain) in worlds {
        let mut rendered = Vec::with_capacity(frames.len());
        for frame in frames {
            rendered.push(RenderFrame::from_frame(frame, registry));
            progress.done += 1;
            redraw_progress(&progress, &mut last, false).await;
        }
        let terrain = terrain.map(|frame| RenderFrame::from_frame(frame, registry));
        result.push((name, FrameSequence::new(rendered).expect("no valid frames found"), terrain));
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
    counter: &mut DrawCallCounter,
) {
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
    draw_text(
        format!(
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
        10.0,
        20.0,
        20.0,
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
    draw_text(
        format!("{} draw calls  |  {detail_text}", counter.calls),
        10.0,
        42.0,
        20.0,
        Color::new(0.6, 0.9, 1.0, 1.0),
    );
    let tab_hint = if world_count > 1 { "  |  tab switches world" } else { "" };
    draw_text(
        format!(
            "drag to pan, scroll to zoom  |  left/right step, space {}, home/end jump  |  -/= speed ({}x)  |  s toggles sprites, l toggles LOD  |  f auto-follows the growing base ({})  |  drag the bar below to scrub{tab_hint}",
            if state.playing { "pause" } else { "play" },
            state.play_speed,
            if follow_enabled { "on" } else { "off" },
        ),
        10.0,
        64.0,
        20.0,
        WHITE,
    );
}

/// Text size for the bar's own labels, a step below the HUD's 20.0: these
/// are reference marks read at a glance beside the bar, not primary readouts.
const TIMELINE_LABEL_SIZE: f32 = 16.0;

/// Elapsed game time at `index`, or an empty string for an index the
/// sequence does not have. Frames carry the real `game.tick` they were
/// emitted at (see `replay::run`), so this is the capture's own clock rather
/// than anything derived from frame numbering.
fn frame_time_label(sequence: &FrameSequence, index: usize) -> String {
    sequence.frames().get(index).map(|frame| format_game_time(frame.tick)).unwrap_or_default()
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
fn draw_timeline_bar(timeline: &Timeline, sequence: &FrameSequence, mouse: Vec2, scrubbing: bool) {
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

    if timeline.contains(mouse) || scrubbing {
        draw_timeline_hover(timeline, sequence, mouse);
    }
}

/// Where the capture starts and ends, anchored under the bar's two ends.
/// These bound everything else on the bar: without them the playhead's time
/// is a number with nothing to be a fraction of.
fn draw_timeline_endpoint_labels(timeline: &Timeline, sequence: &FrameSequence) {
    let dim = Color::new(1.0, 1.0, 1.0, 0.55);
    let baseline = timeline.y + 22.0;

    let start = frame_time_label(sequence, 0);
    draw_text(&start, timeline.left, baseline, TIMELINE_LABEL_SIZE, dim);

    // Right-aligned so it ends flush with the bar rather than starting at
    // it and overhanging into the window edge as the label grows.
    let end = frame_time_label(sequence, sequence.len().saturating_sub(1));
    let end_width = measure_text(&end, None, TIMELINE_LABEL_SIZE as u16, 1.0).width;
    draw_text(&end, timeline.left + timeline.width - end_width, baseline, TIMELINE_LABEL_SIZE, dim);
}

/// The current frame's time, centered over the playhead and clamped the same
/// way the hover tooltip is, since the playhead reaches the same bar ends
/// the cursor does.
fn draw_timeline_playhead_label(timeline: &Timeline, sequence: &FrameSequence, playhead_x: f32) {
    let label = format_game_time(sequence.current().tick);
    let width = measure_text(&label, None, TIMELINE_LABEL_SIZE as u16, 1.0).width;
    let left = Timeline::tooltip_left(playhead_x, width, screen_width());
    draw_text(&label, left, timeline.y - 16.0, TIMELINE_LABEL_SIZE, WHITE);
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
    let box_top = timeline.y - 34.0 - box_height;

    draw_rectangle(box_left, box_top, box_width, box_height, Color::new(0.0, 0.0, 0.0, 0.8));
    draw_rectangle_lines(box_left, box_top, box_width, box_height, 2.0, Color::new(1.0, 1.0, 1.0, 0.35));
    draw_text(&time, box_left + padding, box_top + padding + TIMELINE_LABEL_SIZE, TIMELINE_LABEL_SIZE, WHITE);
    draw_text(
        &counter,
        box_left + padding,
        box_top + padding + TIMELINE_LABEL_SIZE * 2.0 + 4.0,
        TIMELINE_LABEL_SIZE,
        Color::new(1.0, 1.0, 1.0, 0.6),
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
        draw_text(name, screen.x + 12.0, screen.y + 4.0, 18.0, WHITE);
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
                Camera::fit_frames(sequence.frames(), terrain.as_ref(), screen_width(), screen_height());
            let growing_bounds = growing_bounds_per_frame(sequence.frames(), &registry);
            // On by default: opening straight into the fully-zoomed-out
            // whole-sequence fit (see `Camera::fit_frames` above) looks
            // exactly like broken auto-follow (big from the very first
            // frame, never zooming out further) unless auto-follow is
            // already active to immediately pull it in to how small the
            // base actually starts. `f` still toggles it off for anyone who
            // wants full manual control from the start.
            let follow = FollowState { enabled: true, ..Default::default() };
            WorldView { name, sequence, camera, growing_bounds, follow, terrain }
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
    };
    let mut counter = DrawCallCounter::new(BATCH_INDEX_CAPACITY);

    loop {
        // Captured before the mutable borrow below, which holds `worlds`
        // borrowed for the rest of the loop body.
        let world_count = worlds.len();
        if is_key_pressed(KeyCode::Tab) && world_count > 1 {
            current = (current + 1) % world_count;
        }
        let WorldView { name: world_name, sequence, camera, growing_bounds, follow, terrain } =
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

        draw_world(frame, terrain.as_ref(), camera, screen_center, &registry, &sprites, use_sprites, use_lod, &mut counter);

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
        draw_timeline_bar(&timeline, sequence, mouse_position().into(), state.dragging_timeline);
        draw_player_markers(&player_track, world_name, sequence.current().tick, camera, screen_center);

        next_frame().await;
    }
}
