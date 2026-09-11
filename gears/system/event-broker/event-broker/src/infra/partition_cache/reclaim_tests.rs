//! Pure: sequence spans, byte counts and reader positions. No cache, no
//! segment, no event, no runtime. Keep it that way - the clustering rule is
//! the part most likely to be wrong, and it is cheapest to check here.

use crate::domain::model::Sequence;

use super::reclaim::{
    GapThresholdEvents, ReclaimPolicy, ResidencyLimitBytes, RetainedWindows, SegmentSummary, plan,
};

const GAP: GapThresholdEvents = GapThresholdEvents(100);

/// A policy that never applies byte pressure, so a test sees the window rule
/// alone.
fn windows_only() -> ReclaimPolicy {
    ReclaimPolicy::new(GAP, ResidencyLimitBytes(usize::MAX))
}

fn summary(from: i64, through: i64, bytes: usize) -> SegmentSummary {
    SegmentSummary::builder(Sequence::assigned(from))
        .through(Sequence::assigned(through))
        .events(1)
        .bytes(bytes)
        .build()
}

/// The fixtures below number events plainly. These three carry the numbers
/// across the type boundary so each assertion reads as a span rather than as a
/// column of constructor calls.
fn positions(values: &[i64]) -> Vec<Sequence> {
    values.iter().copied().map(Sequence::assigned).collect()
}

fn spans(windows: &RetainedWindows) -> Vec<(i64, i64)> {
    windows
        .windows()
        .iter()
        .map(|(lower, upper)| (lower.as_i64(), upper.as_i64()))
        .collect()
}

fn starts(taken: &[Sequence]) -> Vec<i64> {
    taken.iter().copied().map(Sequence::as_i64).collect()
}

fn intersects(windows: &RetainedWindows, from: i64, through: i64) -> bool {
    windows.intersects(Sequence::assigned(from), Sequence::assigned(through))
}

/// A densely populated partition: segments of fifty events over fifty
/// sequences, so counting events and measuring sequences give the same answer.
/// The window rule is stated against this deliberately - a case where the two
/// readings agree isolates the clustering logic from the counting logic, which
/// `a_sparse_partition_...` then exercises on its own.
fn dense(through: i64) -> Vec<SegmentSummary> {
    (0..)
        .map(|n| (n * 50 + 1, n * 50 + 50))
        .take_while(|(from, _)| *from <= through)
        .map(|(from, upper)| {
            SegmentSummary::builder(Sequence::assigned(from))
                .through(Sequence::assigned(upper))
                .events(50)
                .bytes(50)
                .build()
        })
        .collect()
}

#[test]
fn one_reader_keeps_the_runway_ahead_of_it() {
    let windows = RetainedWindows::from_positions(&dense(400), &positions(&[50]), GAP);

    assert_eq!(spans(&windows), [(50, 150)]);
    assert!(
        !intersects(&windows, 10, 40),
        "behind the reader, and a reader never goes back"
    );
    assert!(intersects(&windows, 60, 80), "inside the runway");
    assert!(
        !intersects(&windows, 200, 300),
        "beyond the runway the reader is entitled to"
    );
}

#[test]
fn readers_within_the_threshold_are_one_cluster() {
    let windows = RetainedWindows::from_positions(&dense(400), &positions(&[50, 120]), GAP);

    // 120 sits inside the window 50 opened, so nothing between them is worth
    // taking and they keep one span - reaching to the end of the segment
    // holding the hundredth event past 120.
    assert_eq!(spans(&windows), [(50, 200)]);
}

#[test]
fn readers_further_apart_than_the_threshold_are_separate_clusters() {
    let windows = RetainedWindows::from_positions(&dense(1200), &positions(&[50, 1000]), GAP);

    assert_eq!(spans(&windows), [(50, 150), (1000, 1100)]);
    assert!(
        !intersects(&windows, 400, 500),
        "the stretch between two clusters is exactly what is reclaimable"
    );
}

#[test]
fn positions_need_not_arrive_sorted_or_unique() {
    let windows =
        RetainedWindows::from_positions(&dense(1200), &positions(&[1000, 50, 50, 120]), GAP);

    assert_eq!(spans(&windows), [(50, 200), (1000, 1100)]);
}

#[test]
fn a_span_below_every_reader_is_dead_and_one_above_is_merely_gapped() {
    let summaries = [
        summary(10, 40, 10),
        // Sixty events each, so the reader at 50 has spent its hundred-event
        // runway before 400 - which is what puts 400 outside every window.
        SegmentSummary::builder(Sequence::assigned(51))
            .through(Sequence::assigned(100))
            .events(60)
            .bytes(10)
            .build(),
        SegmentSummary::builder(Sequence::assigned(101))
            .through(Sequence::assigned(150))
            .events(60)
            .bytes(10)
            .build(),
        summary(400, 500, 10),
    ];

    let decided = plan(&summaries, &positions(&[50, 1000]), &windows_only());

    // The distinction is the price: nobody can ask for 10..=40 again, while
    // 400..=500 will be refetched by the reader at 50 as it catches up.
    assert_eq!(starts(decided.dead()), [10]);
    assert_eq!(starts(decided.gapped()), [400]);
    assert!(!decided.limit_breached());
}

#[test]
fn a_span_inside_a_readers_window_is_left_alone() {
    let summaries = [summary(60, 80, 10), summary(1050, 1080, 10)];

    let decided = plan(&summaries, &positions(&[50, 1000]), &windows_only());

    assert!(decided.is_empty());
}

#[test]
fn with_no_readers_registered_nothing_is_taken_as_dead_or_gapped() {
    let summaries = [summary(10, 40, 10), summary(400, 500, 10)];

    // A partition whose last reader deregistered for an instant during a
    // rebalance must not be flushed; residency is bounded by the byte limit
    // alone until someone registers again.
    let decided = plan(&summaries, &[], &windows_only());

    assert!(decided.is_empty());
}

#[test]
fn byte_pressure_takes_the_most_speculative_span_first() {
    let summaries = [
        summary(51, 100, 100),  // next for the reader at 50
        summary(101, 150, 100), // just ahead
        summary(300, 350, 100), // far ahead: prefetch nobody has needed
    ];
    let policy = ReclaimPolicy::new(GapThresholdEvents(10_000), ResidencyLimitBytes(250));

    let decided = plan(&summaries, &positions(&[50]), &policy);

    assert_eq!(
        starts(decided.pressured()),
        [300],
        "dropping prefetch that has not been needed costs less than dropping \
         what a reader is about to read"
    );
    assert!(!decided.limit_breached());
}

#[test]
fn byte_pressure_never_takes_the_span_a_reader_reads_next() {
    let summaries = [summary(51, 100, 500)];
    let policy = ReclaimPolicy::new(GapThresholdEvents(10_000), ResidencyLimitBytes(10));

    let decided = plan(&summaries, &positions(&[50]), &policy);

    // Taking it would guarantee an immediate refetch of the very thing just
    // discarded, so the limit is reported breached instead of pretended away.
    assert!(decided.is_empty());
    assert!(decided.limit_breached());
}

#[test]
fn pressure_stops_as_soon_as_the_limit_is_met() {
    let summaries = [
        summary(51, 100, 100),
        summary(300, 350, 100),
        summary(400, 450, 100),
    ];
    let policy = ReclaimPolicy::new(GapThresholdEvents(10_000), ResidencyLimitBytes(200));

    let decided = plan(&summaries, &positions(&[50]), &policy);

    assert_eq!(
        decided.pressured().len(),
        1,
        "one is enough to get under 200"
    );
    assert!(!decided.limit_breached());
}

#[test]
fn the_window_rule_and_the_byte_limit_compose() {
    let summaries = [
        summary(10, 40, 100),   // dead
        summary(400, 500, 100), // gapped: past the reader's hundred-event runway
        // Sixty events each, so the runway is spent inside these two.
        SegmentSummary::builder(Sequence::assigned(51))
            .through(Sequence::assigned(100))
            .events(60)
            .bytes(100)
            .build(), // next for the reader at 50
        SegmentSummary::builder(Sequence::assigned(120))
            .through(Sequence::assigned(150))
            .events(60)
            .bytes(100)
            .build(), // ahead of it, and droppable under pressure
    ];
    let policy = ReclaimPolicy::new(GAP, ResidencyLimitBytes(100));

    let decided = plan(&summaries, &positions(&[50]), &policy);

    assert_eq!(starts(decided.dead()), [10]);
    assert_eq!(starts(decided.gapped()), [400]);
    assert_eq!(
        starts(decided.pressured()),
        [120],
        "the window rule frees 200 bytes, and the limit still needs 100 more"
    );
    assert!(!decided.limit_breached());
}

#[test]
fn a_zero_threshold_still_keeps_what_a_reader_reads_next() {
    // A reader at 50 will be fetched for at 51, so 51 must survive a pass no
    // matter how tight the threshold. Reclaiming it would strand the reader:
    // `absorb` wakes the readers a span covers, so it would wake for this span,
    // lose it in the same call, and leave nobody to wake it again.
    let windows =
        RetainedWindows::from_positions(&dense(400), &positions(&[50]), GapThresholdEvents(0));

    assert!(intersects(&windows, 51, 51));
    assert!(
        !intersects(&windows, 10, 40),
        "a zero threshold still keeps nothing the reader has passed"
    );
}

#[test]
fn a_zero_threshold_does_not_strand_a_reader_under_byte_pressure() {
    let summaries = [summary(51, 100, 1000)];
    let policy = ReclaimPolicy::new(GapThresholdEvents(0), ResidencyLimitBytes(1));

    let decided = plan(&summaries, &positions(&[50]), &policy);

    assert!(
        decided.is_empty(),
        "the span a reader reads next is inviolable even when the limit cannot \
         otherwise be met"
    );
    assert!(decided.limit_breached());
}

/// The reason the reach is counted rather than measured. Retention removes
/// prefixes and one partition carries many tenants' events, so a span of
/// sequence numbers is not a number of events: here two thousand sequences
/// hold ten events between them, and a hundred-event runway therefore has to
/// reach across all of them rather than stopping a hundred numbers in.
#[test]
fn a_sparse_partition_keeps_a_runway_of_events_not_of_sequence_numbers() {
    let sparse = vec![
        SegmentSummary::builder(Sequence::assigned(1))
            .through(Sequence::assigned(1000))
            .events(5)
            .bytes(5)
            .build(),
        SegmentSummary::builder(Sequence::assigned(1001))
            .through(Sequence::assigned(2000))
            .events(5)
            .bytes(5)
            .build(),
    ];

    let windows = RetainedWindows::from_positions(&sparse, &positions(&[10]), GAP);

    assert_eq!(
        spans(&windows),
        [(10, 2000)],
        "ten resident events is fewer than the hundred the threshold asks for, so the runway is \
         everything the partition holds - where a sequence distance would have stopped at 110 \
         and reclaimed events the reader had not reached"
    );
    assert!(
        intersects(&windows, 1500, 1600),
        "a span holding events the reader has not read is not reclaimable"
    );
}
