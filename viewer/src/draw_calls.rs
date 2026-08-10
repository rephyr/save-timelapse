//! Mirrors macroquad 0.4's batching rule so the viewer can report the draw
//! calls it actually costs.

use crate::registry::TypeId;

/// `quad_gl.rs::geometry` merges new geometry only into the *immediately
/// preceding* draw call, and starts a fresh one when the bound texture
/// differs or the index buffer would overflow. macroquad's own
/// `telemetry::drawcalls` can't be used for a running count: `track_drawcall`
/// allocates a 128x128 render texture per call, so counting thousands of them
/// would cost more than the thing being measured.
pub struct DrawCallCounter {
    max_indices: usize,
    current: Option<TypeId>,
    started: bool,
    indices: usize,
    pub calls: usize,
    pub quads: usize,
}

impl DrawCallCounter {
    pub const INDICES_PER_QUAD: usize = 6;

    pub fn new(max_indices: usize) -> Self {
        DrawCallCounter { max_indices, current: None, started: false, indices: 0, calls: 0, quads: 0 }
    }

    pub fn reset(&mut self) {
        self.current = None;
        self.started = false;
        self.indices = 0;
        self.calls = 0;
        self.quads = 0;
    }

    /// Record one quad bound to `texture` (`None` for an untextured rect,
    /// which macroquad treats as its own distinct texture state).
    pub fn quad(&mut self, texture: Option<TypeId>) {
        let would_overflow = self.indices + Self::INDICES_PER_QUAD > self.max_indices;
        if !self.started || self.current != texture || would_overflow {
            self.calls += 1;
            self.indices = 0;
            self.current = texture;
            self.started = true;
        }
        self.indices += Self::INDICES_PER_QUAD;
        self.quads += 1;
    }

    /// Record `n` consecutive quads sharing one texture: the batched case,
    /// without looping per quad.
    pub fn quads(&mut self, texture: Option<TypeId>, n: usize) {
        if n == 0 {
            return;
        }
        self.quad(texture);
        let remaining = n - 1;
        let per_call = self.max_indices / Self::INDICES_PER_QUAD;
        let room = per_call - self.indices / Self::INDICES_PER_QUAD;
        let after_fill = remaining.saturating_sub(room);
        self.calls += after_fill.div_ceil(per_call);
        self.indices = if after_fill == 0 {
            self.indices + remaining * Self::INDICES_PER_QUAD
        } else {
            let tail = after_fill % per_call;
            let quads_in_last = if tail == 0 { per_call } else { tail };
            quads_in_last * Self::INDICES_PER_QUAD
        };
        self.quads += remaining;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of grouping: one texture switch per type instead of
    /// one per entity. macroquad only merges into the immediately preceding
    /// draw call, so interleaved types cost a draw call each.
    #[test]
    fn grouping_collapses_draw_calls_against_interleaved_order() {
        let types: Vec<TypeId> = (0..600).map(|i| (i % 3) as TypeId).collect();
        let huge_buffer = usize::MAX / 2;

        let mut interleaved = DrawCallCounter::new(huge_buffer);
        for &t in &types {
            interleaved.quad(Some(t));
        }
        assert_eq!(interleaved.calls, 600, "every switch starts a new draw call");

        let mut grouped = DrawCallCounter::new(huge_buffer);
        let mut sorted = types.clone();
        sorted.sort();
        for &t in &sorted {
            grouped.quad(Some(t));
        }
        assert_eq!(grouped.calls, 3, "one per distinct type");
    }

    /// Untextured rects are their own texture state, so interleaving shapes
    /// and sprites breaks the batch exactly like two different sprites do.
    #[test]
    fn untextured_quads_break_the_batch_like_a_texture_change() {
        let mut counter = DrawCallCounter::new(usize::MAX / 2);
        counter.quad(Some(0));
        counter.quad(None);
        counter.quad(Some(0));
        assert_eq!(counter.calls, 3);
    }

    /// Even perfectly grouped, macroquad's index buffer caps a draw call at
    /// `max_indices / 6` quads: the ceiling that made raising the capacity
    /// worth doing alongside the sorting.
    #[test]
    fn a_full_index_buffer_splits_one_texture_across_draw_calls() {
        let max_indices = 5000; // macroquad's default
        let per_call = max_indices / DrawCallCounter::INDICES_PER_QUAD; // 833
        let mut counter = DrawCallCounter::new(max_indices);
        for _ in 0..per_call * 2 {
            counter.quad(Some(0));
        }
        assert_eq!(counter.calls, 2);

        counter.quad(Some(0));
        assert_eq!(counter.calls, 3, "one past a full buffer starts another call");
    }

    /// The bulk helper is only useful if it counts identically to the
    /// per-quad path it replaces, across buffer boundaries.
    #[test]
    fn bulk_quads_match_counting_one_at_a_time() {
        for n in [0usize, 1, 5, 832, 833, 834, 1666, 1667, 5000] {
            let mut one_by_one = DrawCallCounter::new(5000);
            for _ in 0..n {
                one_by_one.quad(Some(1));
            }
            let mut bulk = DrawCallCounter::new(5000);
            bulk.quads(Some(1), n);
            assert_eq!(bulk.calls, one_by_one.calls, "calls for n={n}");
            assert_eq!(bulk.quads, one_by_one.quads, "quads for n={n}");
        }
    }

    #[test]
    fn bulk_quads_then_a_switch_still_counts_the_switch() {
        let mut counter = DrawCallCounter::new(5000);
        counter.quads(Some(1), 10);
        counter.quads(Some(2), 10);
        counter.quads(Some(1), 10);
        assert_eq!(counter.calls, 3);
        assert_eq!(counter.quads, 30);
    }
}
