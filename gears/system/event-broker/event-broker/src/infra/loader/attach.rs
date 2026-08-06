//! Opening one session's readers on the partitions it was assigned.
//!
//! The seam between a session, which knows an assignment and a set of cursors,
//! and the topic manager, which owns the caches. A session never names a cache:
//! it asks for readers and gets handles.
//!
//! Partitions are created here on first use rather than at bootstrap, because
//! an instance cannot know which partitions it will be assigned until a group
//! joins.

use std::sync::Arc;

use tokio::sync::Notify;

use crate::domain::model::{Assignment, Cursor, Sequence};
use crate::domain::streaming::read::{PartitionRead, ReadLimit};
use crate::domain::streaming::read_set::PartitionSlot;
use crate::domain::streaming::reader::PartitionReader;
use crate::domain::streaming::source::PartitionKey;
use crate::infra::partition_cache::cache::ReaderHandle;

use super::topics::{Partition, TopicManager};

/// What a session needs in order to open its readers.
///
/// A struct rather than four arguments: two of them are slices and would be
/// transposable at a call site, and the whole point of the type is that the
/// caller names what it is passing.
pub struct AttachRequest<'a> {
    pub topics: &'a TopicManager,
    /// Membership only. The offsets carried on `Assignment` are the SDK's and
    /// are deliberately not read here - a session's starting position comes
    /// from the persisted cursor.
    pub assigned: &'a [Assignment],
    pub cursors: &'a [Cursor],
    /// Shared by every reader this call opens, so the session awaits once for
    /// the whole assignment rather than once per partition.
    pub ready: &'a Arc<Notify>,
}

/// One session's hold on one partition: the reader, and the partition it reads.
///
/// Holding the `Arc<Partition>` is what keeps the partition from being retired
/// underneath the session. `TopicManager::retire_idle` drops a partition only
/// when the map is its sole holder *and* it has no registered readers, so this
/// handle satisfies both halves - and it must, because a retired-then-reattached
/// key would build a second cache for the same partition and leave readers on
/// each believing they had the partition's state.
///
/// Deliberately does **not** call `Partition::claim`. That flag is the
/// scheduler's in-flight fetch suppression - one fetch at a time per partition -
/// so a session holding it would suppress fetches for the partition it is
/// waiting on, forever.
struct HeldReader {
    reader: ReaderHandle,
    /// Held for its `Drop`, never read: see the type comment.
    _partition: Arc<Partition>,
}

impl PartitionReader for HeldReader {
    fn has_data(&self) -> bool {
        self.reader.has_data()
    }

    fn read(&self, limit: ReadLimit) -> PartitionRead {
        self.reader.read(limit)
    }

    fn seek(&self, offset: Sequence) {
        self.reader.seek(offset);
    }

    fn report_scanning(&self, scanning: bool) {
        self.reader.report_scanning(scanning);
    }
}

/// The persisted cursor for one partition, or 0 when none exists.
///
/// Zero means "nothing processed yet" rather than "start at sequence 0": ADR-0001
/// fixes sequences as starting from 1 and delivery as `cursor + 1`, so a fresh
/// subscription and a cursor of 0 are the same state.
fn cursor_for(cursors: &[Cursor], key: &PartitionKey) -> Sequence {
    cursors
        .iter()
        .find(|cursor| cursor.topic == key.topic && cursor.partition == key.partition)
        .map_or(Sequence::assigned(0), |cursor| cursor.offset)
}

/// Opens one reader per assignment, attaching partitions that do not exist yet.
///
/// Every reader shares `request.ready`, so an absorb on any of the session's
/// partitions wakes it once (D23).
#[must_use]
pub fn attach_readers(request: &AttachRequest<'_>) -> Vec<PartitionSlot> {
    request
        .assigned
        .iter()
        .map(|assignment| {
            let key = PartitionKey::new(assignment.topic.clone(), assignment.partition);
            let offset = cursor_for(request.cursors, &key);
            let partition = request.topics.attach(&key);
            let reader = partition
                .cache()
                .track_reader_sharing(offset, Arc::clone(request.ready));

            let held = HeldReader {
                reader,
                _partition: partition,
            };
            // `starting_at` seeds both the slot and the reader; the reader was
            // registered at the same offset above, so this is consistent rather
            // than a second source of truth.
            PartitionSlot::new(key, Arc::new(held)).starting_at(offset)
        })
        .collect()
}
