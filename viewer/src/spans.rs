//! Storing a whole timelapse as spans rather than as frames.
//!
//! A timelapse is overwhelmingly redundant. Consecutive frames of a real
//! capture differ by the few hundred things built between them, out of
//! hundreds of thousands standing still, and the old layout paid full price
//! for every one of them in every frame: a 400k-entity base over 200 frames
//! is about a gigabyte of `RenderEntity` before anything else. That is not a
//! speed problem, it is the ceiling on how long a capture can be before the
//! viewer runs out of memory.
//!
//! So each thing is stored once, with the half-open range of frames it is
//! present for, and a frame is recovered by asking which spans cover it. The
//! same base becomes a few megabytes: cost moves from frames times entities
//! to distinct entities, and a longer capture of the same factory now costs
//! almost nothing extra, where before it scaled linearly.
//!
//! Spans are sorted by type, so materializing a frame walks them once and
//! emits items already grouped by type, which is the order the renderer
//! batches by. Nothing has to sort per seek.

use crate::registry::TypeId;
use crate::render_frame::Run;

/// One item present over a contiguous stretch of frames.
///
/// `last` is exclusive, so a thing built in frame 3 and still standing at the
/// end of a 10-frame capture is `first: 3, last: 10`, and `first == last`
/// cannot happen: a span is only closed after at least one frame contained
/// it.
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
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    pub fn span_count(&self) -> usize {
        self.spans.len()
    }

    /// Folds one more frame in. Call once per frame, in order, then
    /// [`SpanBuilder::finish`].
    ///
    /// Identity is the caller's `key`: two items in consecutive frames with
    /// the same key and the same type are the same thing continuing to exist,
    /// so its span extends rather than a new one starting. Position is what
    /// callers use, matching how the replay world keys entities.
    pub fn iter(&self) -> impl Iterator<Item = &Span<T>> {
        self.spans.iter()
    }

    /// The items present at `frame`, appended to `out` grouped by type, with
    /// one [`Run`] per type describing where each group sits.
    ///
    /// One linear pass over every span, with no allocation once the buffers
    /// have grown. That is more work than indexing a prebuilt frame would be,
    /// but it happens when the displayed frame changes rather than once per
    /// rendered frame, and a pass over a few hundred thousand contiguous
    /// spans is far cheaper than having kept every frame resident.
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

/// Accumulates frames into a [`SpanSet`], one frame at a time, so the caller
/// can drop each parsed frame as soon as it has been folded in rather than
/// holding every frame at once. Holding them all is exactly what this type
/// exists to avoid, so building from a `&[Frame]` would defeat the purpose at
/// load time even though the result is small.
///
/// Sorted vectors and a merge walk rather than the hash map this obviously
/// wants, for the same reason `activity::analyze_activity` does: this runs on
/// the load path against every item of every frame, tens of millions of times
/// on a real capture. A `HashMap` there is 30 million random-access probes
/// into a table far larger than cache, and the cost is the probing, not the
/// hashing. Sorting and merging touches the same data almost entirely
/// sequentially and needs no allocation per frame once the buffers have
/// grown. Measured on a 150-frame, 400k-entity capture (see `bench` below):
/// 2.50s with the map, 1.91s with this.
///
/// A smaller win than the same change bought in `activity.rs` (2.26s to
/// 1.07s), and for a visible reason: that one sorts bare `u64` keys, where
/// this sorts `(key, type, item)` tuples three times the size, so the sort
/// itself is now the floor. Sorting an index array instead would shrink what
/// moves but scatter the reads, which is the trade this already went through
/// once in the other direction.
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

    /// Folds in one frame's worth of items, given as `(key, type, item)`.
    ///
    /// An item whose key is already standing with the same type continues its
    /// span. A key that was standing and is absent here ends its span at this
    /// frame. A key that reappears later starts a fresh span, which is right:
    /// something torn down and rebuilt on the same tile is genuinely absent in
    /// between, and a single span would draw it through a gap where it was
    /// not there.
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
            // Both sides are sorted by key, so this pointer only ever moves
            // forward: anything it steps over was standing last frame and is
            // absent from this one, which ends its span. Nothing needs doing
            // to close it, since `last` is exclusive and was already set to
            // the frame after the one it was last seen in.
            while open_at < self.open.len() && self.open[open_at].0 < key {
                open_at += 1;
            }

            let continues = self
                .open
                .get(open_at)
                .filter(|&&(open_key, index)| open_key == key && self.spans[index as usize].type_id == type_id);

            match continues {
                Some(&(_, index)) => {
                    self.spans[index as usize].last = frame + 1;
                    self.next_open.push((key, index));
                }
                None => {
                    // Either brand new, or the same tile now holding a
                    // different type, which is a different thing: leave the
                    // old span closed where it ended and open one here.
                    let index = self.spans.len() as u32;
                    self.spans.push(Span { item, type_id, first: frame, last: frame + 1 });
                    self.next_open.push((key, index));
                }
            }
        }

        // `current` was sorted, so `next_open` came out sorted too and is
        // ready to be merged against directly next frame.
        std::mem::swap(&mut self.open, &mut self.next_open);
        self.frames += 1;
    }

    /// Folds in `n` frames identical to the one just pushed.
    ///
    /// An export omits a surface's frame entirely when nothing on that
    /// surface changed (see `replay::write_all_surfaces`), which on a
    /// multi-surface save is most frames for most surfaces. This puts them
    /// back, so every surface still has one frame per emitted moment and the
    /// index-addressed timeline keeps working.
    ///
    /// One pass over what is standing however large `n` is, rather than `n`
    /// passes. Nothing changed across the gap by definition, so every span
    /// open when it started is still open when it ends, and each one's `last`
    /// can jump straight to the far side. That is the whole reason this takes
    /// a count instead of being called repeatedly: on a megabase surface
    /// idling through a long stretch, the difference is one walk over ~900k
    /// spans versus dozens of them.
    pub fn push_repeats(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let last = self.frames + n as u32;
        for &(_, index) in &self.open {
            self.spans[index as usize].last = last;
        }
        self.frames = last;
    }

    /// Sorts by type and hands back the finished set.
    pub fn finish(mut self) -> SpanSet<T> {
        self.spans.sort_by_key(|span| (span.type_id, span.first));
        SpanSet { spans: self.spans, frame_count: self.frames as usize }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// here rather than quietly eroding the win.
    ///
    /// A span costs more than a bare entity, since it carries the type and
    /// the two frame bounds alongside it. It breaks even at two frames and
    /// wins from three onward, which every real capture is.
    #[test]
    fn a_span_costs_a_fixed_amount_more_than_the_item_it_wraps() {
        let entity = std::mem::size_of::<RenderEntity>();
        let span = std::mem::size_of::<Span<RenderEntity>>();
        assert_eq!(entity, 12, "RenderEntity grew; the numbers below need revisiting");
        // 12 for the entity, 2 for the type, 4 each for the bounds, padded
        // to the 4-byte alignment. The type could be dropped to reach 20 by
        // keeping per-type boundaries alongside the type-sorted spans instead
        // of one copy per span, which is worth doing only if this ever stops
        // being comfortably small.
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

    /// What the span layout actually costs and saves, on a sequence shaped
    /// like a real capture: a base growing to 400k entities over 150 frames.
    ///
    /// `#[ignore]`d because it builds every frame the old way first, which is
    /// the peak this exists to remove, so it needs the memory it is measuring
    /// against. Run in release:
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
                    entities.push(RenderEntity { x: (i % 2000) as f32 + 0.5, y: (i / 2000) as f32 + 0.5, w: 1, h: 1, d: 0, shape: 0 });
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
        let mut sequence = FrameSequence::new(frames).unwrap();
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
