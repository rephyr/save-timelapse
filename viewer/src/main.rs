//! Thin macroquad glue: argument parsing, the window loop, input polling,
//! and drawing. Everything else (camera math, color hashing, synthetic
//! data, the frame sequence, sprite path resolution) lives in `lib.rs`,
//! where it's unit tested -- none of it can be, once it touches macroquad's
//! window/input globals or does real texture loading.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use macroquad::prelude::*;
use save_timelapse::export::install_data_dir;
use save_timelapse::locate::locate_factorio;
use viewer::{color_for, entity_footprint_size, icon_candidates, load_sequence, synthetic_frame, synthetic_tiles, Camera, FrameSequence, Timeline};

const ZOOM_STEP: f32 = 1.1;
const PLAY_INTERVAL_SECS: f32 = 0.25; // ~4 frames/sec auto-play
/// Below this, a sprite is imperceptible and not worth a texture draw over a
/// flat rect -- the zoom-based sprites/shapes split agreed back in the
/// milestone-1 discussion.
const SPRITE_MIN_PIXELS: f32 = 12.0;

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

/// `--synthetic-tiles` is a knob independent of `--synthetic`/a frame path:
/// it layers a synthetic floor on top of whichever entities were requested,
/// since the real risk case (a fully-paved megabase) is tile-heavy in a way
/// the entity-only stress test doesn't cover. Both stay single-frame --
/// playback is for real exported sequences, not synthetic stress tests.
fn load(path_arg: Option<String>, synthetic_entities: Option<usize>, synthetic_tile_count: Option<usize>) -> FrameSequence {
    let mut frame = if let Some(n) = synthetic_entities {
        println!("synthetic frame: {n} entities");
        synthetic_frame(n)
    } else if let Some(path) = &path_arg {
        println!("loading {path}");
        let frames = load_sequence(std::path::Path::new(path)).expect("failed to load frame(s)");
        if let Some(n) = synthetic_tile_count {
            println!("synthetic tiles: {n}");
        }
        let sequence = FrameSequence::new(
            frames
                .into_iter()
                .map(|mut f| {
                    if let Some(n) = synthetic_tile_count {
                        f.tiles = synthetic_tiles(n);
                    }
                    f
                })
                .collect(),
        )
        .expect("no frames found");
        println!("{} frame(s) loaded", sequence.len());
        return sequence;
    } else {
        let default = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frames/frame_0004.json");
        println!("no frame given, defaulting to {default}");
        let text = std::fs::read_to_string(default).expect("failed to read default fixture");
        serde_json::from_str(&text).expect("failed to parse frame JSON")
    };

    if let Some(n) = synthetic_tile_count {
        println!("synthetic tiles: {n}");
        frame.tiles = synthetic_tiles(n);
    }

    FrameSequence::new(vec![frame]).expect("frames is never empty here")
}

/// Every distinct entity/tile name across the whole sequence, so sprites can
/// be loaded once up front rather than stuttering mid-scrub the first time a
/// not-yet-seen type appears.
fn distinct_names(sequence: &FrameSequence) -> HashSet<String> {
    let mut names = HashSet::new();
    for frame in sequence.frames() {
        names.extend(frame.entities.iter().map(|e| e.n.clone()));
        names.extend(frame.tiles.iter().map(|t| t.n.clone()));
    }
    names
}

/// Best-effort: a Factorio install found (given or auto-detected) doesn't
/// mean every icon resolves, since this only covers vanilla/Space-Age
/// naming, not arbitrary mods. Missing icons just mean that type keeps
/// using its colored shape -- never an error.
async fn load_sprites(data_dir: Option<&std::path::Path>, names: &HashSet<String>) -> HashMap<String, Texture2D> {
    let mut sprites = HashMap::new();
    let Some(data_dir) = data_dir else {
        return sprites;
    };
    for name in names {
        for candidate in icon_candidates(data_dir, name) {
            if !candidate.exists() {
                continue;
            }
            let Some(path) = candidate.to_str() else { continue };
            if let Ok(texture) = load_texture(path).await {
                sprites.insert(name.clone(), texture);
                break;
            }
        }
    }
    sprites
}

fn draw_entity(center: Vec2, size: Vec2, color: Color, sprite: Option<&Texture2D>) {
    let top_left = center - size / 2.0;
    match sprite {
        Some(texture) => draw_texture_ex(
            texture,
            top_left.x,
            top_left.y,
            WHITE,
            DrawTextureParams { dest_size: Some(size), ..Default::default() },
        ),
        None => draw_rectangle(top_left.x, top_left.y, size.x, size.y, color),
    }
}

/// Tiles are corner positioned, unlike entities, so `screen` here is the
/// tile's top-left corner rather than its center.
fn draw_tile(screen: Vec2, size: f32, color: Color, sprite: Option<&Texture2D>) {
    match sprite {
        Some(texture) => draw_texture_ex(
            texture,
            screen.x,
            screen.y,
            WHITE,
            DrawTextureParams { dest_size: Some(Vec2::splat(size)), ..Default::default() },
        ),
        None => draw_rectangle(screen.x, screen.y, size, size, color),
    }
}

#[macroquad::main("save-timelapse viewer")]
async fn main() {
    let args = parse_args();
    let mut sequence = load(args.path, args.synthetic_entities, args.synthetic_tile_count);

    let data_dir = args.factorio.or_else(locate_factorio).and_then(|exe| install_data_dir(&exe));
    match &data_dir {
        Some(dir) => println!("factorio data: {}", dir.display()),
        None => println!("no factorio install found (pass --factorio); sprites unavailable, using colored shapes"),
    }
    let names = distinct_names(&sequence);
    let sprites = load_sprites(data_dir.as_deref(), &names).await;
    println!("{} of {} entity/tile types have sprites", sprites.len(), names.len());

    let mut camera = Camera::fit_frames(sequence.frames(), screen_width(), screen_height());
    let mut last_mouse: Vec2 = mouse_position().into();
    let mut playing = false;
    let mut play_accum = 0.0;
    let mut dragging_timeline = false;

    loop {
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
        if playing {
            play_accum += get_frame_time();
            if play_accum >= PLAY_INTERVAL_SECS {
                play_accum = 0.0;
                if sequence.index() + 1 < sequence.len() {
                    sequence.step_forward();
                } else {
                    playing = false;
                }
            }
        }

        clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

        let pixels_per_tile = camera.pixels_per_tile();
        let use_sprites = pixels_per_tile > SPRITE_MIN_PIXELS;
        let frame = sequence.current();

        // Floor first, so buildings drawn afterward sit on top of it.
        let tile_size = pixels_per_tile.max(1.0);
        for tile in &frame.tiles {
            let world = Vec2::new(tile.x as f32, tile.y as f32);
            let screen = camera.world_to_screen(world, screen_center);
            if screen.x < -tile_size
                || screen.y < -tile_size
                || screen.x > screen_width() + tile_size
                || screen.y > screen_height() + tile_size
            {
                continue;
            }
            let sprite = if use_sprites { sprites.get(&tile.n) } else { None };
            draw_tile(screen, tile_size, color_for(&tile.n, 0.35, 0.5), sprite);
        }

        for entity in &frame.entities {
            let world = Vec2::new(entity.x, entity.y);
            let screen = camera.world_to_screen(world, screen_center);
            let size = entity_footprint_size(pixels_per_tile, entity.w, entity.h);
            let margin = size.x.max(size.y);
            if screen.x < -margin
                || screen.y < -margin
                || screen.x > screen_width() + margin
                || screen.y > screen_height() + margin
            {
                continue;
            }
            let sprite = if use_sprites { sprites.get(&entity.n) } else { None };
            draw_entity(screen, size, color_for(&entity.n, 0.55, 0.85), sprite);
        }

        draw_text(
            &format!(
                "frame {}/{}  |  {} entities  |  {} tiles  |  zoom {:.2}x  |  {} fps",
                sequence.index() + 1,
                sequence.len(),
                frame.count,
                frame.tiles.len(),
                camera.zoom,
                get_fps()
            ),
            10.0,
            20.0,
            20.0,
            WHITE,
        );
        draw_text(
            &format!(
                "drag to pan, scroll to zoom  |  left/right step, space {}, home/end jump  |  drag the bar below to scrub",
                if playing { "pause" } else { "play" }
            ),
            10.0,
            42.0,
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

        next_frame().await;
    }
}
