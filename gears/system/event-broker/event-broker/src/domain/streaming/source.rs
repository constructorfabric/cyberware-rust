//! How a session addresses a partition.
//!
//! Only the key lives here now. The read seam is `PartitionReader` (D27), and
//! there is no per-partition factory trait beside it: a session attaches to a
//! whole assignment at once, and the partitions it needs may not exist yet, so
//! only `infra::loader::topics::TopicManager` can create them. A
//! `PartitionSource` trait declaring `open_reader(offset)` stood here briefly
//! and never acquired a caller - `infra::loader::attach::attach_readers` is the
//! attach point, and it works at assignment scope rather than per partition.

use toolkit_gts::GtsInstanceId;

/// One partition of one topic - the unit every read, resident span, and reader
/// registration is scoped to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartitionKey {
    pub topic: GtsInstanceId,
    pub partition: i32,
}

impl PartitionKey {
    #[must_use]
    pub fn new(topic: GtsInstanceId, partition: i32) -> Self {
        Self { topic, partition }
    }
}
