//! One session's reader on one partition - the whole seam between a session and
//! the cache.
//!
//! Shaped like a **file handle**: the reader owns a position, `read` consumes
//! forward from it, and `seek` moves it. That shape is forced rather than
//! chosen. Reclamation has to retain against every live reader's position, so
//! the cache must know where each reader is; a positioned read that keeps no
//! state cannot supply that, and a positioned read *plus* a separately published
//! position - which is what this trait used to be - is the one combination with
//! no advantage: one fact in two representations, on opposite sides of the seam,
//! kept in agreement by convention. Forgetting to publish pinned a partition's
//! memory forever with no diagnostic. A single call that advances what it read
//! cannot be half-performed.
//!
//! Where the file analogy stops: this file is appended to concurrently and has
//! holes that fill in later, so a short read is not EOF. `NothingNew` means
//! "caught up for now" and `Unknown` means "not accounted for yet" - both
//! transient, and neither a reason to close.
//!
//! Readiness is a *check*, not an await, and there is deliberately no `wait`
//! here. A session holds a reader per partition it was assigned and must wake
//! when *any* of them has something; awaiting per reader would cost one future
//! per partition. Every reader of a session shares one waker, the session awaits
//! that once, and then uses these cheap checks to find out which partition it
//! was.
//!
//! A reader does not report its own partition. The cache is per partition, so a
//! handle never needed to carry the key, and the session already holds each
//! reader in a slot beside the key it belongs to - duplicating it here would
//! give two places to disagree.

use crate::domain::model::Sequence;
use crate::domain::streaming::read::{PartitionRead, ReadLimit};

pub trait PartitionReader: Send + Sync {
    /// Whether anything is accounted for past this reader's position.
    ///
    /// Optimistic: it can say yes where a read then reports the position
    /// unanswerable, because the frontier being ahead does not mean the gap in
    /// front of this reader has been filled. The read is authoritative.
    fn has_data(&self) -> bool;

    /// Reads forward from this reader's own position, exclusive, and advances
    /// that position to whatever the read accounted for.
    ///
    /// No offset argument and no companion publish call - see the module note.
    /// The examined frontier *is* the read's `accounted_through`, which the
    /// cache itself produced, so there is nothing for a caller to report back.
    ///
    /// This rests on a read being fully consumed, which is what `ReadLimit`
    /// guarantees: the cap belongs on the read, not on how much of the result a
    /// caller chooses to look at. Were partial consumption ever allowed, the
    /// frontier would stop being derivable here and an explicit
    /// `release_through(frontier)` would have to come back.
    fn read(&self, limit: ReadLimit) -> PartitionRead;

    /// Moves the position, as `lseek` does. Absolute, because every caller -
    /// SEEK, and seeding at open - knows the sequence it wants rather than a
    /// delta, and the only position change that may move *backwards*.
    fn seek(&self, offset: Sequence);

    /// Publishes the session's scanning classification.
    ///
    /// A derived bit the session reports, not a measurement anyone keeps here:
    /// selectivity is the session's, and this is the one bit the allocator needs
    /// so a reader that discards nearly everything is capped rather than given
    /// more runway. It stays a publish precisely because the cache cannot derive
    /// it - unlike the position, which it can.
    fn report_scanning(&self, scanning: bool);
}
