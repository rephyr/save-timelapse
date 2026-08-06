//! Milestone 1 spike: load one exported frame and render it with pan/zoom.
//!
//! The point of this crate isn't the picture, it's the question it answers:
//! can plain macroquad immediate-mode draws stay smooth at the entity and
//! tile counts a real base produces, or does the renderer need real GPU
//! instance buffers sooner rather than later. `--synthetic <n>` and
//! `--synthetic-tiles <n>` exist to load-test past what the real fixtures
//! reach (they top out around 23k entities and carry no tiles at all).

use macroquad::prelude::*;
use save_timelapse::frame::{Entity, Frame, Tile};

const BASE_PIXELS_PER_TILE: f32 = 32.0;
const ZOOM_STEP: f32 = 1.1;

/// Every render goes through here. This is the seam a later milestone hooks
/// into: pick a sprite instead of a shape once zoom and entity type allow it.
fn draw_entity(entity: &Entity, screen: Vec2, pixels_per_tile: f32, color: Color) {
    let size = pixels_per_tile.max(1.0);
    draw_rectangle(screen.x - size / 2.0, screen.y - size / 2.0, size, size, color);
    let _ = entity.d; // orientation is unused until real sprites care about facing
}

/// Tiles are corner positioned, unlike entities, so `screen` here is the
/// tile's top-left corner rather than its center.
fn draw_tile(screen: Vec2, pixels_per_tile: f32, color: Color) {
    let size = pixels_per_tile.max(1.0);
    draw_rectangle(screen.x, screen.y, size, size, color);
}

/// Deterministic name -> color, so a given entity type is always the same
/// color across runs with nothing to curate as new Factorio types show up.
/// Tiles get a dimmer, less saturated palette than entities so the floor
/// layer reads as background rather than competing with buildings.
fn color_for(name: &str, saturation: f32, value: f32) -> Color {
    let mut hash: u32 = 2166136261;
    for b in name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    let hue = (hash % 360) as f32 / 360.0;
    let (r, g, b) = hsv_to_rgb(hue, saturation, value);
    Color::new(r, g, b, 1.0)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Grid of fabricated entities, cycling through a handful of type names, for
/// load-testing at counts the real fixtures don't reach.
fn synthetic_frame(count: usize) -> Frame {
    const NAMES: &[&str] = &[
        "transport-belt",
        "assembling-machine-1",
        "electric-pole",
        "inserter",
        "pipe",
        "splitter",
    ];
    let side = (count as f32).sqrt().ceil() as i64;
    let spacing = 2.0;
    let entities = (0..count)
        .map(|i| {
            let ix = (i as i64) % side;
            let iy = (i as i64) / side;
            Entity {
                n: NAMES[i % NAMES.len()].to_string(),
                x: ix as f32 * spacing,
                y: iy as f32 * spacing,
                d: 0,
            }
        })
        .collect();
    Frame { tick: 0, surface: "synthetic".to_string(), count, entities, tiles: Vec::new() }
}

/// Filled grid of concrete tiles, for load-testing the case a fully-paved
/// megabase produces: far more tile cells than entities.
fn synthetic_tiles(count: usize) -> Vec<Tile> {
    let side = (count as f32).sqrt().ceil() as i64;
    (0..count)
        .map(|i| {
            let ix = (i as i64) % side;
            let iy = (i as i64) / side;
            Tile { n: "concrete".to_string(), x: ix as i32, y: iy as i32 }
        })
        .collect()
}

struct Camera {
    offset: Vec2,
    zoom: f32,
}

impl Camera {
    fn pixels_per_tile(&self) -> f32 {
        BASE_PIXELS_PER_TILE * self.zoom
    }

    fn world_to_screen(&self, world: Vec2, screen_center: Vec2) -> Vec2 {
        screen_center + (world - self.offset) * self.pixels_per_tile()
    }

    fn screen_to_world(&self, screen: Vec2, screen_center: Vec2) -> Vec2 {
        self.offset + (screen - screen_center) / self.pixels_per_tile()
    }

    /// Center on the bounding box of everything in the frame and pick a zoom
    /// that fits it on screen. Real bases are almost never near world origin
    /// (this fixture sits around x=[-38,601], y=[-1109,-470]), so starting at
    /// offset zero would open on empty space for basically every real save.
    /// Tiles are included too: paved area can extend past built entities.
    fn fit(entities: &[Entity], tiles: &[Tile], screen_width: f32, screen_height: f32) -> Camera {
        let mut points = entities
            .iter()
            .map(|e| Vec2::new(e.x, e.y))
            .chain(tiles.iter().map(|t| Vec2::new(t.x as f32, t.y as f32)));
        let Some(first) = points.next() else {
            return Camera { offset: Vec2::ZERO, zoom: 1.0 };
        };
        let mut min = first;
        let mut max = first;
        for p in points {
            min = min.min(p);
            max = max.max(p);
        }
        let center = (min + max) / 2.0;
        let size = (max - min).max(Vec2::splat(1.0));
        let zoom = (screen_width / (size.x * BASE_PIXELS_PER_TILE))
            .min(screen_height / (size.y * BASE_PIXELS_PER_TILE))
            * 0.9;
        Camera { offset: center, zoom: zoom.clamp(0.01, 50.0) }
    }
}

/// `--synthetic-tiles` is a knob independent of `--synthetic`/a frame path:
/// it layers a synthetic floor on top of whichever entities were requested,
/// since the real risk case (a fully-paved megabase) is tile-heavy in a way
/// the entity-only stress test doesn't cover.
fn load_frame() -> Frame {
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

    let mut frame = if let Some(n) = synthetic_entities {
        println!("synthetic frame: {n} entities");
        synthetic_frame(n)
    } else if let Some(path) = path {
        println!("loading {path}");
        let text = std::fs::read_to_string(&path).expect("failed to read frame file");
        serde_json::from_str(&text).expect("failed to parse frame JSON")
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

    frame
}

#[macroquad::main("save-timelapse viewer")]
async fn main() {
    let frame = load_frame();
    println!(
        "{} entities, {} tiles on {} @ tick {}",
        frame.count,
        frame.tiles.len(),
        frame.surface,
        frame.tick
    );

    let colors: Vec<Color> = frame.entities.iter().map(|e| color_for(&e.n, 0.55, 0.85)).collect();
    let tile_colors: Vec<Color> = frame.tiles.iter().map(|t| color_for(&t.n, 0.35, 0.5)).collect();

    let mut camera = Camera::fit(&frame.entities, &frame.tiles, screen_width(), screen_height());
    let mut last_mouse: Vec2 = mouse_position().into();

    loop {
        let screen_center = Vec2::new(screen_width() / 2.0, screen_height() / 2.0);
        let mouse: Vec2 = mouse_position().into();

        if is_mouse_button_down(MouseButton::Left) {
            let delta = mouse - last_mouse;
            camera.offset -= delta / camera.pixels_per_tile();
        }
        last_mouse = mouse;

        let (_, wheel_y) = mouse_wheel();
        if wheel_y != 0.0 {
            let before = camera.screen_to_world(mouse, screen_center);
            camera.zoom *= if wheel_y > 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
            camera.zoom = camera.zoom.clamp(0.01, 50.0);
            let after_offset = before - (mouse - screen_center) / camera.pixels_per_tile();
            camera.offset = after_offset;
        }

        clear_background(Color::new(0.08, 0.08, 0.1, 1.0));

        let pixels_per_tile = camera.pixels_per_tile();

        // Floor first, so buildings drawn afterward sit on top of it.
        for (tile, color) in frame.tiles.iter().zip(&tile_colors) {
            let world = Vec2::new(tile.x as f32, tile.y as f32);
            let screen = camera.world_to_screen(world, screen_center);
            if screen.x < -pixels_per_tile
                || screen.y < -pixels_per_tile
                || screen.x > screen_width() + pixels_per_tile
                || screen.y > screen_height() + pixels_per_tile
            {
                continue;
            }
            draw_tile(screen, pixels_per_tile, *color);
        }

        for (entity, color) in frame.entities.iter().zip(&colors) {
            let world = Vec2::new(entity.x, entity.y);
            let screen = camera.world_to_screen(world, screen_center);
            if screen.x < -pixels_per_tile
                || screen.y < -pixels_per_tile
                || screen.x > screen_width() + pixels_per_tile
                || screen.y > screen_height() + pixels_per_tile
            {
                continue;
            }
            draw_entity(entity, screen, pixels_per_tile, *color);
        }

        draw_text(
            &format!(
                "{} entities  |  {} tiles  |  zoom {:.2}x  |  {} fps  |  drag to pan, scroll to zoom",
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

        next_frame().await;
    }
}
