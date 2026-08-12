//! Storing a whole timelapse as spans rather than as frames.
//!
//! Consecutive frames of a real capture differ by the few hundred things built
//! between them out of hundreds of thousands standing still, and the old
//! layout paid full price for every one in every frame: a 400k-entity base
//! over 200 frames is about a gigabyte before anything else. That was the
//! ceiling on how long a capture could be, not a speed problem.
//!
//! So each thing is stored once with the half-open range of frames it is
//! present for. Cost moves from frames times entities to distinct entities, so
//! a longer capture of the same factory costs almost nothing extra.
//!
//! Spans are sorted by type, so materializing a frame emits items already
//! grouped the way the renderer batches, with nothing to sort per seek.

use crate::registry::TypeId;
use crate::render_frame::Run;

/// One item present over a contiguous stretch of frames. `last` is exclusive,
/// and `first == last` cannot happen, a span only being closed after at least
/// one frame contained it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span<T> {
    pub item: T,
    pub type_id: TypeId,
    pub first: u32,
    pub last: u32,
}

/// Every version of a thing that ever existed, across every frame.
#[derive(Debug)]
pub struct SpanSet<T> {
    /// Sorted by `type_id`, then by `first`. The type ordering is what lets
    /// `materialize` emit runs without sorting; the `first` ordering within a
    /// type is incidental but keeps output stable between runs.
    spans: Vec<Span<T>>,
    frame_count: usize,
}

impl<T: Copy> SpanSet<T> {
    /// A set built directly rather than frame by frame, for the aggregated
    /// layers, which are derived from the item spans once at the end instead
    /// of being maintained alongside them.
    pub fn from_spans(mut spans: Vec<Span<T>>, frame_count: usize) -> SpanSet<T> {
        spans.sort_by_key(|span| (span.type_id, span.first));
        SpanSet { spans, frame_count }
    }

    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    pub fn span_count(&self) -> usize {
        self.spans.len()
    }

    /// Folds one more frame in. Call once per frame, in order, then
    /// [`SpanBuilder::finish`].
    ///
    /// Identity is the caller's `key`: same key and same type in consecutive
    /// frames is the same thing continuing, so its span extends.
    pub fn iter(&self) -> impl Iterator<Item = &Span<T>> {
        self.spans.iter()
    }

    /// The items present at `frame`, appended to `out` grouped by type, with
    /// one [`Run`] per type.
    ///
    /// One linear pass over every span with no allocation once the buffers
    /// have grown. More work than indexing a prebuilt frame, but it happens
    /// when the displayed frame changes rather than once per rendered frame.
    pub fn materialize(&self, frame: usize, out: &mut Vec<T>, runs: &mut Vec<Run>) {
        out.clear();
        runs.clear();
        let frame = frame as u32;

        for span in &self.spans {
            if span.first > frame || span.last <= frame {
                continue;
            }
            match runs.last_mut() {
                // Spans are type-sorted, so a matching type is always the run
                // being built and never one already closed.
                Some(run) if run.type_id == span.type_id => run.end += 1,
                _ => runs.push(Run { type_id: span.type_id, start: out.len() as u32, end: out.len() as u32 + 1 }),
            }
            out.push(span.item);
        }
    }
}

/// Accumulates frames into a [`SpanSet`] one at a time, so the caller can drop
/// each parsed frame as it is folded in. Building from a `&[Frame]` would hold
/// every frame at once, which is exactly what this type exists to avoid.
///
/// Sorted vectors and a merge walk rather than the hash map this obviously
/// wants, for the same reason as `activity::analyze_activity`: on the load
/// path against every item of every frame, a `HashMap` is 30 million
/// random-access probes into a table far larger than cache, and the cost is
/// the probing rather than the hashing. On a 150-frame, 400k-entity capture,
/// 2.50s with the map against 1.91s with this.
///
/// A smaller win than the same change bought in `activity.rs`, which sorts
/// bare `u64` keys where this sorts tuples three times the size, so the sort
/// itself is now the floor.
pub struct SpanBuilder<T> {
    spans: Vec<Span<T>>,
    /// Everything still standing, as `(key, span index)` sorted by key, which
    /// is what lets the next frame merge against it in one pass.
    open: Vec<(u64, u32)>,
    /// Reused across frames so a frame costs no allocation of its own.
    current: Vec<(u64, TypeId, T)>,
    next_open: Vec<(u64, u32)>,
    frames: u32,
}

impl<T: Copy> Default for SpanBuilder<T> {
    fn default() -> Self {
        SpanBuilder { spans: Vec::new(), open: Vec::new(), current: Vec::new(), next_open: Vec::new(), frames: 0 }
    }
}

impl<T: Copy> SpanBuilder<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds in one frame's items, given as `(key, type, item)`.
    ///
    /// A key already standing with the same type continues its span; one that
    /// was standing and is absent ends it; one that reappears later starts a
    /// fresh span, something rebuilt on the same tile being genuinely absent
    /// in between.
    pub fn push_frame(&mut self, items: impl IntoIterator<Item = (u64, TypeId, T)>) {
        let frame = self.frames;

        self.current.clear();
        self.current.extend(items);
        self.current.sort_unstable_by_key(|&(key, _, _)| key);
        // Two items on one key would make the merge below ambiguous about
        // which span continues. Positions are unique in practice, so this is
        // a guard rather than a real case.
        self.current.dedup_by_key(|&mut (key, _, _)| key);

        self.next_open.clear();
        let mut open_at = 0usize;

        for &(key, type_id, item) in &self.current {
            // Both sides are sorted by key, so this only moves forward:
            // anything stepped over was standing last frame and is absent now,
            // which is where its span ends.
            while open_at < self.open.len() && self.open[open_at].0 < key {
                self.spans[self.open[open_at].1 as usize].last = frame;
                open_at += 1;
            }

            let continues = self
                .open
                .get(open_at)
                .filter(|&&(open_key, index)| open_key == key && self.spans[index as usize].type_id == type_id);

            match continues {
                // Nothing to write: an open span's `last` is not read until it
                // closes. See `close`.
                Some(&(_, index)) => self.next_open.push((key, index)),
                None => {
                    // Either brand new, or the same tile now holding a
                    // different type, which is a different thing: end the old
                    // span here and open one alongside it.
                    if let Some(&(open_key, index)) = self.open.get(open_at) {
                        if open_key == key {
                            self.spans[index as usize].last = frame;
                            open_at += 1;
                        }
                    }
                    let index = self.spans.len() as u32;
                    self.spans.push(Span { item, type_id, first: frame, last: frame + 1 });
                    self.next_open.push((key, index));
                }
            }
        }

        // Anything past the last key of this frame was standing and is gone.
        while open_at < self.open.len() {
            self.spans[self.open[open_at].1 as usize].last = frame;
            open_at += 1;
        }

        // `current` was sorted, so `next_open` came out sorted too and is
        // ready to be merged against directly next frame.
        std::mem::swap(&mut self.open, &mut self.next_open);
        self.frames += 1;
    }

    /// Ends the span open at `at`, which was last present in the frame before
    /// `frame`. `last` is exclusive, so it is exactly `frame`.
    fn close(&mut self, at: usize, frame: u32) {
        let index = self.open[at].1;
        self.spans[index as usize].last = frame;
    }

    /// Folds in one frame given only what changed, which is what a delta frame
    /// carries and what this structure has always been shaped like.
    ///
    /// Costs one pass over the change rather than over everything standing.
    /// That is the whole point: on a real megabase a frame changed by about
    /// 200 items out of 4.2 million, and rediscovering that by sorting and
    /// merging the full set every frame was most of the load time.
    ///
    /// `removed` keys that are not standing are ignored, matching how replay
    /// treats a removal for something it never saw.
    pub fn push_delta(&mut self, added: impl IntoIterator<Item = (u64, TypeId, T)>, removed: impl IntoIterator<Item = u64>) {
        let frame = self.frames;

        for key in removed {
            if let Ok(at) = self.open.binary_search_by_key(&key, |&(k, _)| k) {
                self.close(at, frame);
                self.open.remove(at);
            }
        }

        for (key, type_id, item) in added {
            let index = self.spans.len() as u32;
            self.spans.push(Span { item, type_id, first: frame, last: frame + 1 });
            match self.open.binary_search_by_key(&key, |&(k, _)| k) {
                // Something already here: a tile repaved, or an entity
                // replaced. The old one ends where the new one begins.
                Ok(at) => {
                    self.close(at, frame);
                    self.open[at] = (key, index);
                }
                Err(at) => self.open.insert(at, (key, index)),
            }
        }

        self.frames += 1;
    }

    /// Folds in `n` frames identical to the one just pushed.
    ///
    /// An export omits a surface's frame when nothing on it changed, which on
    /// a multi-surface save is most frames for most surfaces. This puts them
    /// back, so the index-addressed timeline keeps working.
    ///
    /// Free however large `n` is: an open span's `last` is not written until
    /// it closes, so a gap where nothing changed is a number.
    pub fn push_repeats(&mut self, n: usize) {
        self.frames += n as u32;
    }

    /// Sorts by type and hands back the finished set.
    pub fn finish(mut self) -> SpanSet<T> {
        // Still standing when the capture ended, so their `last` was never
        // written. Exclusive, so it is the frame count.
        let frames = self.frames;
        for &(_, index) in &self.open {
            self.spans[index as usize].last = frames;
        }
        self.spans.sort_by_key(|span| (span.type_id, span.first));
        SpanSet { spans: self.spans, frame_count: self.frames as usize }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deltas and full frames have to be two ways of saying the same thing,
    /// or a timelapse built from one would differ from the same capture built
    /// from the other. Checked by building both and comparing every span.
    #[test]
    fn a_delta_build_is_identical_to_a_full_build() {
        // A base that grows, has something removed, has a tile repaved with a
        // different type, and then sits still.
        let frames: Vec<Vec<(u64, TypeId)>> = vec![
            vec![(1, 0), (2, 0), (3, 1)],
            vec![(1, 0), (2, 0), (3, 1), (4, 1)],
            vec![(1, 0), (3, 1), (4, 1)],
            vec![(1, 0), (3, 2), (4, 1)],
            vec![(1, 0), (3, 2), (4, 1)],
        ];

        let mut full = SpanBuilder::new();
        for frame in &frames {
            full.push_frame(frame.iter().map(|&(key, type_id)| (key, type_id, key)));
        }
        let full = full.finish();

        let mut delta = SpanBuilder::new();
        let mut standing: Vec<(u64, TypeId)> = Vec::new();
        for frame in &frames {
            let added: Vec<(u64, TypeId, u64)> =
                frame.iter().filter(|e| !standing.contains(e)).map(|&(k, t)| (k, t, k)).collect();
            // A key whose type changed counts as removed and added, which is
            // what the writer will emit and what replay already means by it.
            let removed: Vec<u64> = standing
                .iter()
                .filter(|(k, t)| !frame.contains(&(*k, *t)))
                .map(|&(k, _)| k)
                .filter(|k| !added.iter().any(|a| a.0 == *k))
                .collect();
            delta.push_delta(added, removed);
            standing = frame.clone();
        }
        let delta = delta.finish();

        assert_eq!(delta.frame_count(), full.frame_count());
        let mine: Vec<_> = delta.iter().collect();
        let theirs: Vec<_> = full.iter().collect();
        assert_eq!(mine, theirs, "a delta build must produce exactly the spans a full build does");
    }

    /// The saving is only real if a gap costs nothing. An open span's `last` is
    /// not written until it closes, so repeats are a number rather than a walk.
    #[test]
    fn repeats_do_not_touch_what_is_standing() {
        let mut builder = SpanBuilder::new();
        builder.push_frame([(1u64, 0 as TypeId, 1u64), (2, 0, 2)]);
        builder.push_repeats(1_000_000);
        let set = builder.finish();

        assert_eq!(set.frame_count(), 1_000_001);
        for span in set.iter() {
            assert_eq!(span.first, 0);
            assert_eq!(span.last, 1_000_001, "still standing at the end runs to the end");
        }
    }

    /// Builds a set from frames of `(key, type)` pairs, using the key as the
    /// item too so assertions can name what came back.
    fn build(frames: &[&[(u64, TypeId)]]) -> SpanSet<u64> {
        let mut builder = SpanBuilder::new();
        for frame in frames {
            builder.push_frame(frame.iter().map(|&(key, type_id)| (key, type_id, key)));
        }
        builder.finish()
    }

    fn at(set: &SpanSet<u64>, frame: usize) -> Vec<u64> {
        let (mut out, mut runs) = (Vec::new(), Vec::new());
        set.materialize(frame, &mut out, &mut runs);
        out.sort_unstable();
        out
    }

    #[test]
    fn something_present_throughout_is_stored_once() {
        let set = build(&[&[(1, 0)], &[(1, 0)], &[(1, 0)]]);
        assert_eq!(set.span_count(), 1, "one entity over three frames is one span");
        assert_eq!(set.frame_count(), 3);
        for frame in 0..3 {
            assert_eq!(at(&set, frame), vec![1]);
        }
    }

    #[test]
    fn things_appear_in_the_frame_they_were_built_and_not_before() {
        let set = build(&[&[(1, 0)], &[(1, 0), (2, 0)], &[(1, 0), (2, 0)]]);
        assert_eq!(at(&set, 0), vec![1]);
        assert_eq!(at(&set, 1), vec![1, 2]);
        assert_eq!(at(&set, 2), vec![1, 2]);
    }

    #[test]
    fn things_disappear_in_the_frame_they_were_removed() {
        let set = build(&[&[(1, 0), (2, 0)], &[(1, 0)], &[(1, 0)]]);
        assert_eq!(at(&set, 0), vec![1, 2]);
        assert_eq!(at(&set, 1), vec![1]);
        assert_eq!(at(&set, 2), vec![1]);
    }

    /// A gap has to stay a gap. One span covering both stretches would draw
    /// the thing through frames where it genuinely was not there.
    #[test]
    fn a_rebuilt_thing_is_two_spans_with_a_hole_between_them() {
        let set = build(&[&[(1, 0)], &[], &[(1, 0)]]);
        assert_eq!(set.span_count(), 2);
        assert_eq!(at(&set, 0), vec![1]);
        assert_eq!(at(&set, 1), Vec::<u64>::new(), "the gap must be empty");
        assert_eq!(at(&set, 2), vec![1]);
    }

    /// Same tile, different building. Continuing the span would keep drawing
    /// the old type for the rest of the capture.
    #[test]
    fn replacing_one_type_with_another_on_the_same_key_starts_a_new_span() {
        let mut builder = SpanBuilder::new();
        builder.push_frame([(7u64, 0 as TypeId, 100u64)]);
        builder.push_frame([(7u64, 1 as TypeId, 200u64)]);
        let set = builder.finish();

        assert_eq!(set.span_count(), 2);
        assert_eq!(at(&set, 0), vec![100]);
        assert_eq!(at(&set, 1), vec![200]);
    }

    /// The property the renderer depends on: everything of one type lands in
    /// one contiguous run, so batching survives materialization.
    #[test]
    fn materialized_items_are_grouped_into_one_run_per_type() {
        let set = build(&[&[(1, 0), (2, 1), (3, 0), (4, 1), (5, 2)]]);
        let (mut out, mut runs) = (Vec::new(), Vec::new());
        set.materialize(0, &mut out, &mut runs);

        assert_eq!(runs.len(), 3, "one run per type, got {runs:?}");
        assert!(runs.windows(2).all(|w| w[0].type_id < w[1].type_id), "runs must be type-ordered");
        for run in &runs {
            assert_eq!(run.end - run.start, 2 - u32::from(run.type_id == 2), "run {run:?}");
        }
        // Every run's slice must actually hold that run's items.
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn materializing_reuses_the_buffers_rather_than_growing_them() {
        let set = build(&[&[(1, 0), (2, 0)], &[(1, 0)]]);
        let (mut out, mut runs) = (Vec::new(), Vec::new());
        set.materialize(0, &mut out, &mut runs);
        assert_eq!(out.len(), 2);
        set.materialize(1, &mut out, &mut runs);
        assert_eq!(out.len(), 1, "the previous frame's items must not linger");
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn an_empty_capture_materializes_to_nothing() {
        let set = build(&[]);
        assert_eq!(set.frame_count(), 0);
        assert_eq!(at(&set, 0), Vec::<u64>::new());
    }

    /// The whole point, stated as a number: storage tracks distinct things,
    /// not frames times things.
    #[test]
    fn storage_does_not_grow_with_frame_count() {
        let steady: Vec<(u64, TypeId)> = (0..500).map(|k| (k, (k % 7) as TypeId)).collect();
        let short = build(&[&steady, &steady]);
        let long = build(&vec![steady.as_slice(); 200]);

        assert_eq!(short.span_count(), 500);
        assert_eq!(long.span_count(), 500, "100x the frames must not cost 100x the memory");
        assert_eq!(long.frame_count(), 200);
    }
}

#[cfg(test)]
mod layout {
    use super::*;
    use crate::render_frame::RenderEntity;

    /// The trade in bytes, pinned so a field added to either side shows up
    /// here rather than eroding the win. A span carries the type and two frame
    /// bounds, so it breaks even at two frames and wins from three onward.
    #[test]
    fn a_span_costs_a_fixed_amount_more_than_the_item_it_wraps() {
        let entity = std::mem::size_of::<RenderEntity>();
        let span = std::mem::size_of::<Span<RenderEntity>>();
        assert_eq!(entity, 12, "RenderEntity grew; the numbers below need revisiting");
        // 12 for the entity, 2 for the type, 4 each for the bounds, padded to
        // 4-byte alignment. The type could be dropped to reach 20 by keeping
        // per-type boundaries alongside the sorted spans, worth doing only if
        // this stops being comfortably small.
        assert_eq!(span, 24, "Span<RenderEntity> grew; likewise");

        // A 400k-entity base held for 200 frames, the shape this replaced.
        let per_frame_storage = 200usize * 400_000 * entity;
        let span_storage = 400_000 * span;
        assert!(
            per_frame_storage / span_storage >= 100,
            "expected at least a 100x reduction, got {}x",
            per_frame_storage / span_storage
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::registry::TypeRegistry;
    use crate::render_frame::{FrameSequence, RenderEntity, RenderFrame, Run};

    /// What the span layout costs and saves on a sequence shaped like a real
    /// capture: 400k entities over 150 frames.
    ///
    /// `#[ignore]`d because it builds every frame the old way first, which is
    /// the peak this removes. Run in release:
    ///
    /// ```text
    /// cargo test --release -p viewer --lib gains -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn gains() {
        let frames_n = 150usize;
        let peak_entities = 400_000usize;
        let mut registry = TypeRegistry::new();
        let types: Vec<_> = (0..40).map(|i| registry.intern(&format!("type-{i}"))).collect();

        let mut frames = Vec::with_capacity(frames_n);
        let mut per_frame_items = 0usize;
        for f in 0..frames_n {
            // Grows steadily, and everything already built stays built, which
            // is what a factory does and what makes frames so redundant.
            let n = peak_entities * (f + 1) / frames_n;
            per_frame_items += n;
            let mut entities = Vec::with_capacity(n);
            let mut runs = Vec::with_capacity(types.len());
            for (t, &type_id) in types.iter().enumerate() {
                let start = entities.len() as u32;
                for i in (t..n).step_by(types.len()) {
                    entities.push(RenderEntity {
                        x: (i % 2000) as f32 + 0.5,
                        y: (i / 2000) as f32 + 0.5,
                        w: 1,
                        h: 1,
                        d: 0,
                        shape: 0,
                    });
                }
                runs.push(Run { type_id, start, end: entities.len() as u32 });
            }
            let mut frame = RenderFrame::empty();
            frame.tick = f as u64 * 3600;
            frame.count = entities.len();
            frame.entities = entities;
            frame.entity_runs = runs;
            frames.push(frame);
        }

        let entity_bytes = std::mem::size_of::<RenderEntity>();
        let span_bytes = std::mem::size_of::<Span<RenderEntity>>();
        let old_bytes = per_frame_items * entity_bytes;

        let start = std::time::Instant::now();
        let mut sequence = FrameSequence::new(frames, &TypeRegistry::new()).unwrap();
        let build = start.elapsed();

        let new_bytes = sequence.span_estimate() * span_bytes;

        let start = std::time::Instant::now();
        for i in 0..frames_n {
            sequence.goto(i);
        }
        let seeks = start.elapsed();

        let start = std::time::Instant::now();
        sequence.for_each_frame(|_, _, _| {});
        let walk = start.elapsed();

        println!("GAINS frames={frames_n} peak_entities={peak_entities}");
        println!("GAINS per-frame item copies : {per_frame_items}");
        println!("GAINS distinct spans        : {}", sequence.span_estimate());
        println!("GAINS memory old            : {:.1} MB", old_bytes as f64 / 1e6);
        println!("GAINS memory new            : {:.1} MB", new_bytes as f64 / 1e6);
        println!("GAINS reduction             : {:.1}x", old_bytes as f64 / new_bytes as f64);
        println!("GAINS build spans           : {build:?}");
        println!("GAINS {frames_n} sequential seeks : {seeks:?} ({:?}/seek)", seeks / frames_n as u32);
        println!("GAINS full walk (load pass) : {walk:?}");
    }
}
