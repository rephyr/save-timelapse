//! Thin macroquad glue: argument parsing, the window loop, input polling,
//! and drawing. Everything else (camera math, color hashing, synthetic
//! data, the frame sequence) lives in `lib.rs`, where it's unit tested --
//! none of it can be, once it touches macroquad's window/input globals.

use macroquad::prelude::*;
use viewer::{color_for, load_sequence, synthetic_frame, synthetic_tiles, Camera, FrameSequence, Timeline};

const ZOOM_STEP: f32 = 1.1;
const PLAY_INTERVAL_SECS: f32 = 0.25; // ~4 frames/sec auto-play

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

fn parse_args() -> (Option<String>, Option<usize>, Option<usize>) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = None;
    let mut synthetic_entities = None;
    let mut synthetic_tile_count = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--synthetic" => {
                i += 1;
                synthetic_entities = Some(args.get(i).and_then(|s| s.parse().ok()).unwrap_or(500_000));
            }
            "--synthetic-tiles" => {
                i += 1;
                synthetic_tile_count = Some(args.get(i).and_then(|s| s.parse().ok()).unwrap_or(500_000));
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }

    (path, synthetic_entities, synthetic_tile_count)
}

fn draw_entity(screen: Vec2, pixels_per_tile: f32, color: Color) {
    let size = pixels_per_tile.max(1.0);
    draw_rectangle(screen.x - size / 2.0, screen.y - size / 2.0, size, size, color);
}

/// Tiles are corner positioned, unlike entities, so `screen` here is the
/// tile's top-left corner rather than its center.
fn draw_tile(screen: Vec2, pixels_per_tile: f32, color: Color) {
    let size = pixels_per_tile.max(1.0);
    draw_rectangle(screen.x, screen.y, size, size, color);
}

#[macroquad::main("save-timelapse viewer")]
async fn main() {
    let (path_arg, synthetic_entities, synthetic_tile_count) = parse_args();
    let mut sequence = load(path_arg, synthetic_entities, synthetic_tile_count);

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
        let frame = sequence.current();

        // Floor first, so buildings drawn afterward sit on top of it.
        for tile in &frame.tiles {
            let world = Vec2::new(tile.x as f32, tile.y as f32);
            let screen = camera.world_to_screen(world, screen_center);
            if screen.x < -pixels_per_tile
                || screen.y < -pixels_per_tile
                || screen.x > screen_width() + pixels_per_tile
                || screen.y > screen_height() + pixels_per_tile
            {
                continue;
            }
            draw_tile(screen, pixels_per_tile, color_for(&tile.n, 0.35, 0.5));
        }

        for entity in &frame.entities {
            let world = Vec2::new(entity.x, entity.y);
            let screen = camera.world_to_screen(world, screen_center);
            if screen.x < -pixels_per_tile
                || screen.y < -pixels_per_tile
                || screen.x > screen_width() + pixels_per_tile
                || screen.y > screen_height() + pixels_per_tile
            {
                continue;
            }
            draw_entity(screen, pixels_per_tile, color_for(&entity.n, 0.55, 0.85));
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
