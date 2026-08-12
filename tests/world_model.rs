//! Random play sequences applied to `World` and to a plain reference model,
//! asserting the two agree on what a frame shows.
//!
//! Every world-state bug found so far (ore destroyed by rotating what stands on
//! it, a removal resolving to the ore instead of the building, a buried
//! duplicate outliving the thing that buried it) was a short sequence of
//! ordinary actions that no hand-written test happened to cover. The unit tests
//! in `world.rs` pin those exact sequences; this covers the ones nobody has
//! thought of.
//!
//! The reference is deliberately naive: a list per position and nothing else.
//! It states the intent (a position keeps what is on it, the structure counts
//! as being on top of the ore, a frame shows the top) without reproducing the
//! slab, the free list or the two-deep `under` map, so agreeing with it is
//! evidence rather than a tautology.

use std::collections::BTreeMap;

use save_timelapse::event::Event;
use save_timelapse::frame::{Entity, Frame};
use save_timelapse::world::World;

const ORES: [&str; 2] = ["iron-ore", "coal"];
const BUILDINGS: [&str; 3] = ["transport-belt", "inserter", "electric-mining-drill"];
/// Small enough that positions collide constantly, which is the whole point:
/// sharing a tile is the case that breaks.
const GRID: i32 = 4;

/// xorshift64*, so a failing sequence is reproducible from its seed alone and
/// the suite needs no dependency to generate one.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Placed {
    name: String,
    id: Option<u64>,
    d: u8,
}

impl Placed {
    fn is_ore(&self) -> bool {
        ORES.contains(&self.name.as_str())
    }

    /// The same entity arriving again rather than a second one at the same
    /// spot, which is what a rotation is.
    fn is_same_as(&self, name: &str, id: Option<u64>) -> bool {
        match (self.id, id) {
            (Some(a), Some(b)) => a == b,
            _ => self.name == name,
        }
    }
}

/// What the world should hold, stated as plainly as it can be.
#[derive(Default)]
struct Reference {
    at: BTreeMap<(i32, i32), Vec<Placed>>,
}

impl Reference {
    fn add(&mut self, name: &str, pos: (i32, i32), id: Option<u64>, d: u8) {
        let here = self.at.entry(pos).or_default();
        match here.iter_mut().find(|p| p.is_same_as(name, id)) {
            Some(existing) => {
                existing.d = d;
                existing.id = id.or(existing.id);
            }
            None => here.push(Placed { name: name.to_string(), id, d }),
        }
    }

    /// By id wherever it sits, else whatever is on top of that position, which
    /// is all a removal carrying only a position can mean.
    fn remove(&mut self, pos: (i32, i32), id: Option<u64>) {
        if let Some(id) = id {
            for here in self.at.values_mut() {
                if let Some(i) = here.iter().position(|p| p.id == Some(id)) {
                    here.remove(i);
                    return;
                }
            }
        }
        let Some(here) = self.at.get_mut(&pos) else { return };
        if let Some(i) = Self::top_index(here) {
            here.remove(i);
        }
    }

    /// The structure, never the ore it stands on. This is the whole intent of
    /// the covering rule, and the thing a removal has to resolve to.
    fn top_index(here: &[Placed]) -> Option<usize> {
        here.iter().position(|p| !p.is_ore()).or(if here.is_empty() { None } else { Some(here.len() - 1) })
    }

    /// One entry per occupied position: the covered thing is invisible.
    fn visible(&self) -> Vec<(String, i32, i32, u8)> {
        let mut out: Vec<(String, i32, i32, u8)> = self
            .at
            .iter()
            .filter_map(|(&(x, y), here)| {
                let top = &here[Self::top_index(here)?];
                Some((top.name.clone(), x, y, top.d))
            })
            .collect();
        out.sort();
        out
    }

    fn total(&self) -> usize {
        self.at.values().map(Vec::len).sum()
    }

    fn building_at(&self, pos: (i32, i32)) -> Option<&Placed> {
        self.at.get(&pos)?.iter().find(|p| !p.is_ore())
    }

    fn has_ore_at(&self, pos: (i32, i32)) -> bool {
        self.at.get(&pos).is_some_and(|here| here.iter().any(Placed::is_ore))
    }
}

/// Sorted `(name, x, y, d)` for a surface, at the fixed-point scale positions
/// are keyed by, so the two models are compared on the same terms.
fn shown(world: &World) -> Vec<(String, i32, i32, u8)> {
    let frame = world.to_frame("nauvis", 0);
    let mut out: Vec<(String, i32, i32, u8)> =
        frame.entities.iter().map(|e| (e.n.to_string(), (e.x * 10.0).round() as i32, (e.y * 10.0).round() as i32, e.d)).collect();
    out.sort();
    out
}

fn world_pos(pos: (i32, i32)) -> (f32, f32) {
    (pos.0 as f32 / 10.0, pos.1 as f32 / 10.0)
}

/// Positions land on tile centres, the only place Factorio puts a 1x1 entity,
/// so both models key them identically.
fn pick_pos(rng: &mut Rng) -> (i32, i32) {
    ((rng.below(GRID as usize) as i32) * 10 + 5, (rng.below(GRID as usize) as i32) * 10 + 5)
}

/// One playthrough's worth of legal actions. Only moves a player could
/// actually make: a building is placed on an empty tile, rotated where it
/// stands, or mined. Two different machines never share a tile, because
/// Factorio does not allow it and the format has no way to describe it.
fn run_one(seed: u64, steps: usize) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
    let mut reference = Reference::default();
    let mut world = World::new();
    world.set_resources(ORES.iter().map(|s| s.to_string()).collect());

    // A baseline of ore, plus some of it already built on, which is what a
    // capture switched on mid-playthrough looks like. Order within the frame is
    // arbitrary and must not decide anything.
    let mut baseline = Vec::new();
    for _ in 0..GRID * 2 {
        let pos = pick_pos(&mut rng);
        let (x, y) = world_pos(pos);
        if !reference.has_ore_at(pos) {
            let ore = ORES[rng.below(ORES.len())];
            baseline.push(Entity { n: ore.into(), x, y, d: 0, w: 1, h: 1 });
            reference.add(ore, pos, None, 0);
        }
        if reference.building_at(pos).is_none() && rng.below(2) == 0 {
            let name = BUILDINGS[rng.below(BUILDINGS.len())];
            baseline.push(Entity { n: name.into(), x, y, d: 0, w: 1, h: 1 });
            reference.add(name, pos, None, 0);
        }
    }
    if rng.below(2) == 0 {
        baseline.reverse();
    }
    let count = baseline.len();
    world.load_baseline(&Frame {
        tick: 1,
        surface: "nauvis".to_string(),
        count,
        entities: baseline,
        tiles: Vec::new(),
        floor_unchanged: false,
        ..Default::default()
    });

    check(&world, &reference, seed, 0);

    let mut next_id = 1u64;
    for step in 1..=steps {
        let pos = pick_pos(&mut rng);
        let (x, y) = world_pos(pos);
        let standing = reference.building_at(pos).cloned();

        let event = match (&standing, rng.below(3)) {
            // Rotate what is already there: the same entity arriving again.
            (Some(here), 0) => {
                let d = (here.d + 4) % 16;
                reference.add(&here.name, pos, here.id, d);
                Event::AddEntity { name: here.name.clone(), x, y, d, w: 1, h: 1, id: here.id }
            }
            // Mine it. A building from the baseline has no id replay knows, so
            // Factorio's real unit_number resolves to nothing and the removal
            // falls back to the position.
            (Some(here), _) => {
                let reported = here.id.or(Some(900_000 + step as u64));
                reference.remove(pos, here.id);
                Event::RemoveEntity { id: reported, pos: (x, y), name: None }
            }
            // Nothing there: build something.
            (None, _) => {
                let name = BUILDINGS[rng.below(BUILDINGS.len())];
                let id = Some(next_id);
                next_id += 1;
                let d = (rng.below(4) * 4) as u8;
                reference.add(name, pos, id, d);
                Event::AddEntity { name: name.to_string(), x, y, d, w: 1, h: 1, id }
            }
        };

        world.apply(Some("nauvis"), &event);
        check(&world, &reference, seed, step);
    }
}

fn check(world: &World, reference: &Reference, seed: u64, step: usize) {
    let where_ = format!("seed {seed}, step {step}");

    assert_eq!(shown(world), reference.visible(), "frame disagrees with the reference at {where_}");

    // Nothing may be lost or duplicated behind what is visible, which is what
    // an evicted ore and a buried second copy of a belt each looked like.
    assert_eq!(world.entity_count(), reference.total(), "world holds a different number of things at {where_}");

    let surface = world.surface("nauvis").expect("a surface");
    let mut seen: Vec<(i32, i32, &str)> = surface
        .entities()
        .map(|e| (((e.x * 10.0).round() as i32), ((e.y * 10.0).round() as i32), world.names().name(e.name)))
        .collect();
    seen.sort_unstable();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len(), "two of one prototype at one position at {where_}");
}

/// The blocker item on the beta checklist: coverage of the sequences nobody
/// wrote a test for. Seeds are fixed so a failure is reproducible and a fix
/// can be shown to hold.
#[test]
fn random_play_agrees_with_a_plain_model_of_what_a_tile_holds() {
    for seed in 0..400 {
        run_one(seed, 120);
    }
}

/// Longer runs on fewer seeds, so slots get freed and reused many times over
/// and a stale index has room to show itself.
#[test]
fn long_runs_do_not_drift() {
    for seed in 0..20 {
        run_one(1_000 + seed, 2_000);
    }
}
