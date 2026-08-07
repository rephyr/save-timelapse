//! Thin macroquad glue: argument parsing, the window loop, input polling,
//! and drawing. Everything else (camera math, color hashing, the type
//! registry, frame grouping, the draw-call model, synthetic data, the frame
//! sequence, sprite path resolution) lives in `lib.rs`, where it's unit
//! tested -- none of it can be, once it touches macroquad's window/input
//! globals or does real texture loading.

use std::path::PathBuf;
use std::time::Instant;

use macroquad::prelude::*;
use save_timelapse::export::install_data_dir;
use save_timelapse::locate::locate_factorio;
use viewer::{
    color_for, entity_footprint_size, icon_path, icon_source_rect, synthetic_frame, synthetic_tiles,
    use_chunk_lod, Camera, DrawCallCounter, FrameSequence, LoadProgress, PlayerTrack, ProgressBar,
    RenderFrame, Timeline, TypeRegistry, LOD_CELL_TILES,
};

const ZOOM_STEP: f32 = 1.1;
const PLAY_INTERVAL_SECS: f32 = 0.25; // ~4 frames/sec auto-play
/// Below this, a sprite is imperceptible and not worth a texture draw over a
/// flat rect -- the zoom-based sprites/shapes split agreed back in the
/// milestone-1 discussion.
const SPRITE_MIN_PIXELS: f32 = 12.0;

/// macroquad starts a new GPU draw call whenever its batch buffer fills, so
/// the default capacity (10,000 vertices / 5,000 indices) caps a draw call at
/// 833 quads -- meaning even perfectly texture-sorted output costs a draw call
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

// ---------------------------------------------------------------------------
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
/// since the last redraw -- so a fast load doesn't pay a vsync wait per item.
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
/// the entity-only stress test doesn't cover. Both stay single-frame --
/// playback is for real exported sequences, not synthetic stress tests.
/// One timeline per surface, named, so the caller can switch between them
/// (tab, in the running viewer) instead of only ever seeing whichever one
/// happened to be busiest.
async fn load_frames(args: &Args, registry: &mut TypeRegistry) -> Vec<(String, FrameSequence)> {
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

    let worlds: Vec<(String, Vec<save_timelapse::frame::Frame>)> = if let Some(mut frame) = single {
        if let Some(n) = args.synthetic_tile_count {
            frame.tiles = synthetic_tiles(n);
        }
        vec![(frame.surface.clone(), vec![frame])]
    } else {
        let path = args.path.as_ref().expect("checked above");
        println!("loading {path}");
        let paths =
            viewer::frame_paths(std::path::Path::new(path)).expect("failed to enumerate frames");

        // Reading and parsing each file is independent work, so it happens
        // across every available core rather than one file at a time -- on a
        // real megabase capture this is the dominant cost of opening the
        // viewer. Converting to a RenderFrame needs a single, consistently
        // numbered TypeRegistry, so that part stays sequential below.
        progress.total = paths.len();
        progress.detail = format!(
            "{} core(s)",
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        );
        let load = viewer::ParallelFrameLoad::start(paths);
        let loaded = loop {
            progress.done = load.done();
            redraw_progress(&progress, &mut last, true).await;
            if let Some(loaded) = load.poll() {
                break loaded;
            }
        };
        progress.done = progress.total;
        redraw_progress(&progress, &mut last, true).await;

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
    };

    progress.phase = "converting frames";
    progress.done = 0;
    progress.total = worlds.iter().map(|(_, frames)| frames.len()).sum();
    let mut result = Vec::with_capacity(worlds.len());
    for (name, frames) in worlds {
        let mut rendered = Vec::with_capacity(frames.len());
        for frame in frames {
            rendered.push(RenderFrame::from_frame(frame, registry));
            progress.done += 1;
            redraw_progress(&progress, &mut last, false).await;
        }
        result.push((name, FrameSequence::new(rendered).expect("no valid frames found")));
    }
    redraw_progress(&progress, &mut last, true).await;

    println!(
        "{} world(s) loaded ({} frame(s) total), {} distinct type(s)",
        result.len(),
        result.iter().map(|(_, seq)| seq.len()).sum::<usize>(),
        registry.len()
    );
    result
}

/// A loaded icon plus the region of it that's the actual icon. Vanilla and
/// Space Age icon files are a mipmap strip -- the full-size icon followed by
/// progressively smaller copies -- not a single image, so drawing the whole
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
/// using its colored shape -- never an error.
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

// ---------------------------------------------------------------------------
// Drawing

fn draw_entity(center: Vec2, size: Vec2, color: Color, sprite: Option<&Sprite>) {
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
                ..Default::default()
            },
        ),
        None => draw_rectangle(top_left.x, top_left.y, size.x, size.y, color),
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

/// One level-of-detail cell: always a flat rect, never a sprite -- a chunk
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

#[macroquad::main(window_conf)]
async fn main() {
    let args = parse_args();

    let mut registry = TypeRegistry::new();
    let loaded = load_frames(&args, &mut registry).await;

    // Absent entirely (an older capture, or nobody was connected during
    // capture) is normal, not an error -- no markers drawn, nothing else
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
    let mut worlds: Vec<(String, FrameSequence, Camera)> = loaded
        .into_iter()
        .map(|(name, sequence)| {
            let camera = Camera::fit_frames(sequence.frames(), screen_width(), screen_height());
            (name, sequence, camera)
        })
        .collect();
    let mut current = 0usize;

    let mut last_mouse: Vec2 = mouse_position().into();
    let mut playing = false;
    let mut play_accum = 0.0;
    let mut play_speed: f32 = 1.0;
    let mut dragging_timeline = false;
    let mut sprites_enabled = true;
    let mut lod_enabled = true;
    let mut counter = DrawCallCounter::new(BATCH_INDEX_CAPACITY);

    loop {
        // Captured before the mutable borrow below, which holds `worlds`
        // borrowed for the rest of the loop body.
        let world_count = worlds.len();
        if is_key_pressed(KeyCode::Tab) && world_count > 1 {
            current = (current + 1) % world_count;
        }
        let (world_name, sequence, camera) = &mut worlds[current];

        let screen_center = Vec2::new(screen_width() / 2.0, screen_height() / 2.0);
        let mouse: Vec2 = mouse_position().into();
        let timeline = Timeline::for_screen(screen_width(), screen_height());

        // Which a drag does depends on where it started: grabbing the
        // scrub bar seeks, anywhere else pans the camera, same as a video
        // player's scrubber taking priority over the content behind it.
        if is_mouse_button_pressed(MouseButton::Left) {
            dragging_timeline = timeline.contains(mouse);
        }

        if is_mouse_button_down(MouseButton::Left) {
            if dragging_timeline {
                sequence.goto(timeline.index_for_x(mouse.x, sequence.len()));
                playing = false;
            } else {
                let delta = mouse - last_mouse;
                camera.offset -= delta / camera.pixels_per_tile();
            }
        }
        last_mouse = mouse;

        let (_, wheel_y) = mouse_wheel();
        if wheel_y != 0.0 {
            let before = camera.screen_to_world(mouse, screen_center);
            camera.zoom *= if wheel_y > 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
            camera.zoom = camera.zoom.clamp(0.01, 50.0);
            camera.offset = before - (mouse - screen_center) / camera.pixels_per_tile();
        }

        if is_key_pressed(KeyCode::Right) || is_key_pressed(KeyCode::Period) {
            sequence.step_forward();
            playing = false;
        }
        if is_key_pressed(KeyCode::Left) || is_key_pressed(KeyCode::Comma) {
            sequence.step_back();
            playing = false;
        }
        if is_key_pressed(KeyCode::Home) {
            sequence.goto(0);
            playing = false;
        }
        if is_key_pressed(KeyCode::End) {
            sequence.goto(sequence.len() - 1);
            playing = false;
        }
        if is_key_pressed(KeyCode::Space) {
            playing = !playing;
            play_accum = 0.0;
        }
        // Doubling/halving rather than a linear step: matches how video
        // players commonly expose speed, and keeps the displayed value a
        // clean power of two (0.25x, 0.5x, 1x, 2x, 4x, 8x) instead of an
        // arbitrary decimal.
        if is_key_pressed(KeyCode::Equal) {
            play_speed = (play_speed * 2.0).min(8.0);
        }
        if is_key_pressed(KeyCode::Minus) {
            play_speed = (play_speed / 2.0).max(0.25);
        }
        // Sprites off is the A/B for what texture binding costs: same
        // geometry, one flat-rect batch instead of one batch per type.
        if is_key_pressed(KeyCode::S) {
            sprites_enabled = !sprites_enabled;
        }
        // LOD off is the A/B for what per-item CPU cost costs at extreme
        // zoom-out: forces full-detail rendering even below the chunk
        // threshold, so the difference is directly comparable.
        if is_key_pressed(KeyCode::L) {
            lod_enabled = !lod_enabled;
        }
        if playing {
            // A `while`, not `if`: at high speed multipliers more than one
            // interval's worth can accumulate between two frames (e.g. at
            // 8x and a 60fps display, ~8 frames advance in the time one
            // used to), and stepping only once per tick would cap the
            // visible playback rate at the display's refresh rate instead
            // of the requested speed.
            play_accum += get_frame_time() * play_speed;
            while play_accum >= PLAY_INTERVAL_SECS {
                play_accum -= PLAY_INTERVAL_SECS;
                if sequence.index() + 1 < sequence.len() {
                    sequence.step_forward();
                } else {
                    playing = false;
                    play_accum = 0.0;
                    break;
                }
            }
        }

        clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

        let pixels_per_tile = camera.pixels_per_tile();
        let use_sprites = sprites_enabled && pixels_per_tile > SPRITE_MIN_PIXELS;
        let use_lod = lod_enabled && use_chunk_lod(pixels_per_tile);
        let frame = sequence.current();
        let (view_min, view_max) = view_bounds(&camera, screen_center);
        counter.reset();

        if use_lod {
            // Below LOD_MAX_TILE_PIXELS a full-detail tile or entity is
            // already sub-pixel, so nothing is lost by collapsing a whole
            // LOD_CELL_TILES-square chunk to one quad -- and everything is
            // gained: a chunk grid over the same world area this base
            // actually spans is thousands of quads, not millions, which is
            // the difference between paying a per-item CPU cost 3.4 million
            // times a frame and paying it a few thousand times.
            //
            // Precomputed once at load (`RenderFrame::from_frame`), not
            // here: binning millions of items into chunks is itself too
            // slow to redo every rendered frame, which is exactly the cost
            // this path exists to avoid.
            let chunk_px = pixels_per_tile * LOD_CELL_TILES as f32;
            for run in &frame.tile_lod_runs {
                let color = registry.tile_color(run.type_id);
                let mut drawn = 0;
                for cell in &frame.tile_lod[run.range()] {
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
            // Floor first, so buildings drawn afterward sit on top of it.
            //
            // Iterating runs rather than raw items is what keeps the batch
            // intact: the sprite and color are decided once per type, so
            // macroquad sees a long stretch of quads sharing one texture
            // instead of a texture change per item.
            let tile_size = pixels_per_tile.max(1.0);
            for run in &frame.tile_runs {
                let sprite = if use_sprites { sprites[run.type_id as usize].as_ref() } else { None };
                let color = registry.tile_color(run.type_id);
                let mut drawn = 0;
                for tile in &frame.tiles[run.range()] {
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

            for run in &frame.entity_runs {
                let sprite = if use_sprites { sprites[run.type_id as usize].as_ref() } else { None };
                let color = registry.entity_color(run.type_id);
                let mut drawn = 0;
                for entity in &frame.entities[run.range()] {
                    let half_w = entity.w as f32 / 2.0;
                    let half_h = entity.h as f32 / 2.0;
                    if entity.x + half_w < view_min.x
                        || entity.x - half_w > view_max.x
                        || entity.y + half_h < view_min.y
                        || entity.y - half_h > view_max.y
                    {
                        continue;
                    }
                    let screen = camera.world_to_screen(Vec2::new(entity.x, entity.y), screen_center);
                    let size = entity_footprint_size(pixels_per_tile, entity.w as u32, entity.h as u32);
                    draw_entity(screen, size, color, sprite);
                    drawn += 1;
                }
                counter.quads(sprite.map(|_| run.type_id), drawn);
            }
        }

        let total_items = frame.entities.len() + frame.tiles.len();
        draw_text(
            &format!(
                "[{} {}/{}]  frame {}/{}  |  {} entities  |  {} tiles  |  zoom {:.2}x  |  {} fps  |  {:.1} ms",
                world_name,
                current + 1,
                world_count,
                sequence.index() + 1,
                sequence.len(),
                frame.count,
                frame.tiles.len(),
                camera.zoom,
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
        // only -- macroquad's text rendering adds a few calls of its own.
        //
        // "of {total} ({culled})" only makes sense against full-detail item
        // counts, so it's specific to that branch -- in LOD mode `quads` is
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
            &format!("{} draw calls  |  {detail_text}", counter.calls),
            10.0,
            42.0,
            20.0,
            Color::new(0.6, 0.9, 1.0, 1.0),
        );
        let tab_hint = if world_count > 1 { "  |  tab switches world" } else { "" };
        draw_text(
            &format!(
                "drag to pan, scroll to zoom  |  left/right step, space {}, home/end jump  |  -/= speed ({play_speed}x)  |  s toggles sprites, l toggles LOD  |  drag the bar below to scrub{tab_hint}",
                if playing { "pause" } else { "play" }
            ),
            10.0,
            64.0,
            20.0,
            WHITE,
        );

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

        // Where the player(s) were as of the currently displayed tick, on
        // whichever world/surface is active -- looked up fresh each draw
        // rather than cached on the frame, since it's a cheap scan over a
        // tiny sample count (see PlayerTrack).
        for (name, x, y) in player_track.positions_at(world_name, sequence.current().tick) {
            let screen = camera.world_to_screen(Vec2::new(x, y), screen_center);
            let color = color_for(name, 0.7, 0.95);
            draw_circle(screen.x, screen.y, 9.0, Color::new(0.0, 0.0, 0.0, 0.6));
            draw_circle(screen.x, screen.y, 6.0, color);
            draw_text(name, screen.x + 12.0, screen.y + 4.0, 18.0, WHITE);
        }

        next_frame().await;
    }
}
