//! The exported frame format written by the mod (see mod/control.lua) and
//! consumed by the viewer. Kept in the lib so both can share it.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Frame {
    pub tick: u64,
    pub surface: String,
    pub entities: Vec<Entity>,
    pub count: usize,
    /// Absent in frames captured before tile export existed, hence the
    /// default rather than requiring it.
    #[serde(default)]
    pub tiles: Vec<Tile>,
}

#[derive(Debug, Deserialize)]
pub struct Entity {
    pub n: String,
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub d: u8,
}

/// Unlike entities, tiles are corner positioned and integer aligned: a tile
/// named at (x,y) occupies world space [x,x+1) x [y,y+1).
#[derive(Debug, Deserialize)]
pub struct Tile {
    pub n: String,
    pub x: i32,
    pub y: i32,
}
