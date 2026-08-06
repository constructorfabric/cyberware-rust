//! Operator-facing configuration for the Event Broker module
//! (`DESIGN.md` §4.1 "Deployment Modes", which carries the example YAML).

use std::time::Duration;

use gts::GtsTypeId;
use serde::Deserialize;
use toolkit_utils::iso8601_duration::Iso8601Duration;

/// Transport-level safety cap on `infra::dispatcher::proxy_client::proxy`'s
/// raw, not-yet-typed proxied request body - bounds a read that bypasses any
/// typed extractor entirely, so none of axum's own implicit per-extractor
/// body limits apply to it. Set to axum's own default `Bytes`/`Json`/
/// `String`/`Form` extractor limit (2 MiB) so the dispatcher ends up with the
/// same bound every other handler in this codebase already has implicitly,
/// not a new, arbitrary number. Not operator-configurable - a safety
/// backstop, not a business rule - and distinct from
/// [`BatchConfig::max_payload_bytes`], which is a domain-level rule sized
/// specifically for `events:batch`, not a general transport-safety cap (the
/// dispatcher forwards both ingest and delivery routes, not just batch
/// publishes; see that field's own doc comment for the reverse
/// cross-reference).
pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Which of the four deployment-mode wiring variants this instance runs as.
/// Drives [`crate::module::EventBrokerModule`]'s per-mode service/route
/// gating (`DESIGN.md:2224`'s Deployment Modes table;
/// `docs/ADR/0007-service-decomposition.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    Standalone,
    ClusterIngest,
    ClusterDelivery,
    ClusterDispatcher,
}

/// The `[modules.event_broker]` operator config section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventBrokerConfig {
    pub mode: DeploymentMode,

    /// The GTS backend type a topic's events are stored by when its own
    /// settings name none - a type derived from
    /// `gts.cf.core.events.backend.v1~`, registered by the plugin that serves
    /// it (`DESIGN.md`, "Backend Type vs. Backend Instance").
    ///
    /// A type identifier rather than a short alias, so a deployment names the
    /// same thing here that a topic's `backend.type` names, and a misspelling
    /// fails to load rather than resolving to whichever backend happened to be
    /// linked in.
    pub default_storage_backend: GtsTypeId,

    #[serde(default)]
    pub producer: ProducerConfig,
    #[serde(default)]
    pub batch: BatchConfig,
    #[serde(default)]
    pub polling: PollingConfig,
    #[serde(default)]
    pub subscription: SubscriptionConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub loader: LoaderConfig,
    #[serde(default)]
    pub workers: WorkersConfig,
    #[serde(default)]
    pub registration: RegistrationConfig,
    /// Per-topic deployment settings, keyed by topic GTS identifier. Empty by
    /// default, which makes every topic unresolvable - a deployment that serves
    /// any topic configures at least the instance-less key for its type.
    #[serde(default)]
    pub topics: TopicSettingsMap,
}

/// Deduplication state the broker keeps on behalf of chained and monotonic
/// producers. This is publish-side bookkeeping, unrelated to how long a topic's
/// events are kept.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerConfig {
    /// How long a `(producer_id, topic, partition)` chain row survives without
    /// activity before the reaper deletes it, which is also the window a replayed
    /// publish can still be deduplicated within. Capped at `P14D`: a chain row is
    /// only useful for as long as a producer might retry, and holding one per
    /// producer per partition indefinitely is unbounded state.
    pub state_retention: Iso8601Duration,
}

impl Default for ProducerConfig {
    fn default() -> Self {
        Self {
            // The cap itself: 14 days, in the largest unit `Duration` offers on
            // stable.
            state_retention: Iso8601Duration::new(Duration::from_hours(336)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchConfig {
    pub max_size: u32,
    /// Domain-level, operator-configurable ceiling for `events:batch`
    /// (`DESIGN.md` §2.2) - distinct from
    /// [`MAX_REQUEST_BODY_BYTES`](crate::config::MAX_REQUEST_BODY_BYTES),
    /// the dispatcher's fixed transport-level safety cap on the proxied body
    /// it forwards, which this field does not back (the dispatcher forwards
    /// more than just batch publishes).
    pub max_payload_bytes: u64,
}

impl Default for BatchConfig {
    fn default() -> Self {
        // `DESIGN.md` §2.2 Constraints: 100 events / 1 MiB batch hard limit.
        Self {
            max_size: 100,
            max_payload_bytes: 1_048_576,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollingConfig {
    pub default_timeout_secs: u32,
    pub max_timeout_secs: u32,
}

impl Default for PollingConfig {
    fn default() -> Self {
        // `DESIGN.md` §2.2 Constraints: 30s long-poll max timeout.
        Self {
            default_timeout_secs: 30,
            max_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionConfig {
    /// ISO 8601 duration (e.g. `"PT30S"`); parsing lives with the real
    /// config-loading implementation, not this skeleton.
    pub default_session_timeout: String,
    pub min_session_timeout: String,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            default_session_timeout: "PT30S".to_owned(),
            min_session_timeout: "PT1S".to_owned(),
        }
    }
}

/// Consumption-stream cadence (`GET /v1/events:stream`/`:sse`).
/// `event-broker-consumption-frames`'s own wording is explicit that the
/// heartbeat cadence is "a constant *configurable* idle cadence (default
/// 5s)" - not a fixed constant - since some intermediaries/load balancers
/// need a shorter cadence to keep an idle connection open than the default
/// assumes (matching gRPC's own per-channel-configurable keepalive
/// interval). Wired into `DeliveryServiceImpl::with_heartbeat_interval` when
/// `module.rs` constructs the service (lands with this change's Group 10 -
/// production routing wiring).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingConfig {
    /// Idle cadence. Any frame resets it, so a delivering stream never pays for
    /// a heartbeat. Default 5.
    pub heartbeat_interval_secs: u32,
    /// Events one read may return. Bounds a session's working set, and with the
    /// byte bound below is what stops one slow consumer pulling a partition's
    /// whole tail into memory. Default 256.
    pub read_batch_max_events: usize,
    /// Bytes one read may return. Both bounds apply; whichever binds first
    /// wins. Default 1 MiB.
    pub read_batch_max_bytes: usize,
    /// Events a partition may examine without delivering, before a
    /// `control:progress` frame is owed. Roughly four full read batches, so a
    /// stream whose filter rejects everything still reports where it has got
    /// to. Default 1000.
    ///
    /// Counted as the filter rejects them rather than taken as the distance
    /// between two sequence numbers: sequences are assigned contiguously but
    /// not populated contiguously, so that distance is not a number of events.
    pub progress_drift_threshold: usize,
    /// Floor between two `control:progress` frames. Without it a heavily
    /// filtered stream would emit one per batch, each carrying almost no new
    /// information. Default 30.
    pub progress_min_interval_secs: u32,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_secs: 5,
            read_batch_max_events: 256,
            read_batch_max_bytes: 1024 * 1024,
            progress_drift_threshold: 1000,
            progress_min_interval_secs: 30,
        }
    }
}

/// The per-instance loader: what fills partition caches ahead of readers, and
/// what bounds the memory it holds.
///
/// None of these existed in any configuration struct before - the loader was
/// constructed from literals in tests and benchmarks only, so every value here
/// was previously unreachable by an operator.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoaderConfig {
    /// Concurrent backend reads across every partition on the instance.
    /// Default 16.
    ///
    /// Shared deliberately rather than per partition: one fetch serves every
    /// reader standing at the same position, so the pool bounds latency rather
    /// than the work sent to storage.
    pub pool_permits: usize,
    /// Events one backend fetch may return. Default 256.
    pub fetch_max_events: usize,
    /// How long the loader waits before looking again when a round issued no
    /// fetch. Default 50.
    ///
    /// A floor on latency rather than the mechanism for it. Pacing matters:
    /// an unpaced version of this loop was measured spinning and starving the
    /// tasks it exists to serve.
    pub tick_ms: u64,
    /// Shortest wait before re-polling a partition whose tail has not
    /// materialised. Default 1.
    pub poll_floor_ms: u64,
    /// Longest such wait. Default 64.
    ///
    /// A cluster notification can arrive before the backend has assigned the
    /// sequence, so an empty tail fetch is expected rather than exceptional and
    /// the poller is what keeps a parked reader live.
    pub poll_ceiling_ms: u64,
    /// Added to a demand's rank per round it went unserved. Default 10.
    ///
    /// Additive, not multiplicative, which is what makes eventual service a
    /// guarantee: credit grows independently of how few readers a demand
    /// serves, so any demand eventually outranks any fixed fan-out. Zero is
    /// pure fan-out and can starve a lagging reader indefinitely.
    pub starvation_weight: usize,
    /// Bytes one partition's resident segments may occupy. Default 8 MiB.
    pub residency_limit_bytes: usize,
    /// How wide a gap between reader clusters must be before the span between
    /// them is dropped, in events. Default 16384.
    ///
    /// Dropping it costs the lagging cluster a refetch and buys back the memory
    /// between them; never zero in effect, since a window always keeps at least
    /// the sequence a reader wants next.
    pub gap_threshold_events: usize,
}

impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            pool_permits: 16,
            fetch_max_events: 256,
            tick_ms: 50,
            poll_floor_ms: 1,
            poll_ceiling_ms: 64,
            starvation_weight: 10,
            residency_limit_bytes: 8 * 1024 * 1024,
            gap_threshold_events: 16_384,
        }
    }
}

/// Broker-owned background workers: `reaper` (expired subscriptions +
/// idempotency-key cleanup) and the retention tick.
///
/// The retention knob is a cadence only. `DESIGN.md` §3.7 Key Invariants
/// states the storage backend owns all event deletion, and it still does - the
/// broker decides when a backend performs a pass and what bounds it must end
/// within, never which rows go (see
/// `docs/ADR/0007-service-decomposition.md`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersConfig {
    pub reaper_interval_secs: u32,
    /// How often the ingest role re-reads `types-registry` and re-resolves what
    /// it holds. Default 60.
    ///
    /// A cadence, not a bound: a topic registered while the process is running
    /// becomes resolvable within one tick. Sixty seconds because nothing
    /// registers a topic at runtime today - `types-registry` commits
    /// configuration-seeded entities once, at boot - so the interval covers an
    /// operator using its REST API, not a hot path.
    pub specification_refresh_interval_secs: u32,
    /// How often each topic's backend is driven through one retention pass.
    /// Default 60.
    ///
    /// A cadence, not a bound: a partition may sit past its bounds for up to
    /// one tick, which is nothing against the days and megabytes those bounds
    /// are expressed in. Shorter buys a tighter ceiling at the cost of
    /// re-scanning partitions that were within their bounds a moment ago.
    pub retention_interval_secs: u32,
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            reaper_interval_secs: 60,
            specification_refresh_interval_secs: 60,
            retention_interval_secs: 60,
        }
    }
}

/// `DirectoryService` self-registration address in `cluster_ingest`/
/// `cluster_delivery` mode (`eb-dispatcher-routing` design.md D5) - unused
/// in `standalone`/`cluster_dispatcher` mode. Mirrors `grpc-hub`'s
/// `listen_addr`/`advertise_addr` pattern
/// (`gears/system/grpc-hub/src/gear.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RegistrationConfig {
    /// The address this instance's REST listener binds to.
    pub listen_addr: String,
    /// The address other instances (the dispatcher, in cluster mode) reach
    /// this one at, if different from `listen_addr`. Accepted forms:
    /// `host:<u16>` (literal host and port; `:0` means "use the actual
    /// bound port") or `host` alone (the actual bound port is appended).
    /// `None` falls back to `listen_addr` unless that's a wildcard bind
    /// (`0.0.0.0`), which fails `init` in cluster mode rather than
    /// registering an unroutable address.
    #[serde(default)]
    pub advertise_addr: Option<String>,
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:0".to_owned(),
            advertise_addr: None,
        }
    }
}

/// Partitions a topic has when no configuration entry states a count.
///
/// Configuration always carries an entry for the instance-less key of the topic
/// base type, and this is the count it supplies: a deployment that names no
/// topics at all still serves every topic registered against it. An entry an
/// operator writes overrides this, and a topic's own entry overrides both.
///
/// Eight rather than one: a partition count cannot be changed on a live topic,
/// so the value that needs no thought errs towards leaving room for a consumer
/// group to parallelise rather than towards collapsing it.
pub const BUILT_IN_PARTITIONS: i32 = 8;

/// Default retention duration when nothing states one.
/// A week: long enough that a consumer group down over a weekend still
/// catches up, short enough that an unattended process does not fill a disk.
pub const DEFAULT_RETENTION_DURATION: std::time::Duration = std::time::Duration::from_hours(24 * 7);

/// Why a topic's settings could not be resolved.
///
/// Every variant is a startup-time operator mistake, not a runtime condition:
/// resolution reads configuration only, so the same topic either always
/// resolves or never does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TopicSettingsError {
    /// A partition count that cannot describe a partitioned topic.
    #[error("topic '{topic}' configures {partitions} partitions; the minimum is 1")]
    PartitionsOutOfRange { topic: String, partitions: i32 },

    /// The topic identifier does not decompose into a type and an instance
    /// part, so no instance-less key could be derived from it.
    #[error("topic identifier '{topic}' is not a well-formed GTS instance id: {reason}")]
    MalformedTopicId { topic: String, reason: String },

    /// Two entries select or configure the storage backend differently, and one
    /// instance runs one backend.
    #[error(
        "topics '{first}' and '{second}' name different backends, and this instance runs one backend for every topic"
    )]
    BackendsDisagree { first: String, second: String },
}

/// Per-topic deployment settings, keyed by topic GTS identifier.
///
/// A key whose instance part is empty (`gts.cf.core.events.topic.v1~`) supplies
/// the settings for every topic of that type; a fully qualified key
/// (`gts.cf.core.events.topic.v1~cf.billing.usage.v1`) supplies them for that
/// one topic and takes precedence.
///
/// Both live in the same map, distinguished only by whether the key names an
/// instance, because that is already how GTS expresses "the type itself"
/// rather than an instance of it - a separate `defaults` block would put the
/// default somewhere structurally different from the overrides it backs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct TopicSettingsMap {
    pub entries: std::collections::BTreeMap<String, TopicSettingsEntry>,
}

/// One entry in [`TopicSettingsMap`], exactly as written.
///
/// Every field is optional so that a fully qualified entry overrides only what
/// it names and inherits the rest from the instance-less entry for its type -
/// the common case is a topic that needs more partitions and nothing else, and
/// making it restate the whole backend block would be a place for the two to
/// drift.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopicSettingsEntry {
    /// Partitions this topic has. Unstated here means this entry says nothing
    /// about the count, so resolution falls through to the instance-less entry
    /// for the topic's type and then to [`BUILT_IN_PARTITIONS`] - never to one,
    /// which would silently collapse a topic's parallelism while every consumer
    /// of it still looked healthy.
    #[serde(default)]
    pub partitions: Option<i32>,
    /// How this topic's partitions are kept bounded. Each bound resolves on its
    /// own, so an entry may raise a duration without restating a byte bound.
    /// That is only expressible because [`RetentionSize`] distinguishes "this
    /// entry says nothing" from "this topic is explicitly unbounded"; with a
    /// bare `Option` the second would be unsayable against a bounded default.
    ///
    /// Written at the topic level rather than under `backend` because the broker
    /// resolves the effective bound itself: a topic may declare a retention of
    /// its own and an operator may override it, so the value cannot live inside
    /// a document only one provider understands. Enforcing it is the backend's.
    #[serde(default)]
    pub retention: Option<RetentionSettingsEntry>,
    /// The backend that stores this topic's events. Inherited whole from the
    /// type default when unset, because a backend type and the settings beside
    /// it are one statement: inheriting one backend's settings into another's
    /// selection would hand a provider keys it never defined.
    #[serde(default)]
    pub backend: Option<BackendSettingsEntry>,
}

/// A topic entry's `backend` block, exactly as written: which backend, and
/// whatever that backend's own settings are.
///
/// ```yaml
/// backend:
///   type: gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~
///   path: /var/lib/event-broker/event_log.db
/// ```
///
/// No `deny_unknown_fields` here, and it is not an omission: everything beside
/// `type` belongs to the backend, whose own configuration type rejects a key it
/// does not define. Denying unknown fields at this level would put the gear in
/// charge of what a backend it knows nothing about may be configured with, and
/// `serde` cannot combine that attribute with a flattened remainder anyway.
#[derive(Debug, Clone, Deserialize)]
pub struct BackendSettingsEntry {
    /// The GTS backend type that stores this topic's events. Required whenever
    /// the block is written at all: an entry that names a backend and leaves out
    /// which one is a mistake rather than a request for the default. Omit the
    /// whole block to take [`EventBrokerConfig::default_storage_backend`].
    pub r#type: GtsTypeId,
    /// Everything else in the block, held as written and handed to the backend
    /// verbatim.
    #[serde(flatten)]
    pub settings: serde_json::Map<String, serde_json::Value>,
}

/// A topic entry's `retention` block, exactly as written.
///
/// Every field is optional in the sense that matters for resolution: an absent
/// one is a statement about nothing, not a statement of a default value. Only
/// the resolved [`RetentionSettings`] carries concrete bounds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionSettingsEntry {
    /// Oldest event a partition may hold, as a humantime string (`30d`, `12h`).
    /// Unstated here falls through to the entry for the topic's type, then to
    /// the topic's own declared retention, and last to
    /// [`DEFAULT_RETENTION_DURATION`].
    #[serde(default, with = "toolkit_utils::humantime_serde::option")]
    pub duration: Option<std::time::Duration>,
    /// Bytes a partition may hold. Omit the key to state nothing about the
    /// bound; write `null` to state that this topic has none.
    ///
    /// The unit is in the name because the value carries none. A human-readable
    /// form (`128mb`) is the intended end state and will arrive with a shared
    /// platform helper, the way `duration` already rides `humantime`; until that
    /// exists, an unqualified integer would be ambiguous.
    ///
    /// What the figure counts is the event payload plus the projected columns,
    /// not the space the database finally occupies. Indexes and per-row storage
    /// overhead sit on top of it and are deliberately unaccounted, so a bound of
    /// 128,000,000 permits a file meaningfully larger than that - by roughly
    /// half again. The bound exists to stop unbounded growth, not to predict a
    /// file size.
    #[serde(default, deserialize_with = "deserialize_retention_size")]
    pub size_bytes: RetentionSize,
}

/// What an entry says about a partition's byte bound.
///
/// Three states rather than an `Option`, because two of them are the same value
/// and different statements: a topic inheriting a bounded default and a topic
/// released from it both end up unbounded, but only the second overrides
/// anything. An `Option` would make the release unsayable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RetentionSize {
    /// The key was absent. Resolution looks at the next source.
    #[default]
    Unstated,
    /// The key was present and `null`. This topic is bounded by duration alone,
    /// whatever a broader entry says.
    Unbounded,
    /// The key named a byte count.
    Bytes(u64),
}

impl RetentionSize {
    /// Whether this entry made a statement at all, which is what decides
    /// between using it and falling through to the next source.
    #[must_use]
    pub fn is_stated(self) -> bool {
        !matches!(self, Self::Unstated)
    }

    /// The bound as the backend takes it: `None` is unbounded.
    #[must_use]
    pub fn bytes(self) -> Option<u64> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Unstated | Self::Unbounded => None,
        }
    }
}

/// Absent is handled by `serde(default)`, so reaching here means the key was
/// written: `null` is a deliberate "no bound", and a number is the bound.
fn deserialize_retention_size<'de, D>(deserializer: D) -> Result<RetentionSize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<u64>::deserialize(deserializer)? {
        None => RetentionSize::Unbounded,
        Some(bytes) => RetentionSize::Bytes(bytes),
    })
}

/// How a partition is kept bounded, fully resolved.
///
/// Both bounds apply per partition, independently of every other partition, and
/// whichever is reached first triggers removal. Removal always takes the oldest
/// sequences first, so what remains is a suffix of what was stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetentionSettings {
    pub duration: std::time::Duration,
    pub size_bytes: Option<u64>,
}

/// One topic's settings, fully resolved.
///
/// The product of [`EventBrokerConfig::topic_settings`], never written by an
/// operator: every field is concrete, so a caller holding one needs no second
/// place to look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSettings {
    pub partitions: i32,
    pub retention: RetentionSettings,
    pub backend: BackendSettings,
}

/// One topic's backend selection, fully resolved: which backend, and the
/// settings only that backend understands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackendSettings {
    pub r#type: GtsTypeId,
    pub settings: serde_json::Map<String, serde_json::Value>,
}

impl EventBrokerConfig {
    /// The one backend this instance runs, and the settings it is built from.
    ///
    /// Each topic names its backend in its own entry, but the instance runs a
    /// single backend for all of them, so every entry that names one has to be
    /// describing the same one. Two entries that differ - in the type or in the
    /// settings beside it - are refused rather than resolved by picking one,
    /// because the topic whose entry lost would have its events stored somewhere
    /// its own configuration never named. An entry with no `backend` block says
    /// nothing and never conflicts.
    ///
    /// No entry naming a backend at all yields
    /// [`Self::default_storage_backend`] with no settings, and what that means
    /// is the backend's to decide - the `SQLite` backend reads it as an event
    /// log in memory.
    ///
    /// # Errors
    /// [`TopicSettingsError::BackendsDisagree`] when two entries name different
    /// backends.
    pub fn backend_selection(&self) -> Result<BackendSettings, TopicSettingsError> {
        let mut chosen: Option<(&String, &BackendSettingsEntry)> = None;
        for (key, entry) in &self.topics.entries {
            let Some(block) = entry.backend.as_ref() else {
                continue;
            };
            match chosen {
                None => chosen = Some((key, block)),
                Some((first, first_block))
                    if first_block.r#type != block.r#type
                        || first_block.settings != block.settings =>
                {
                    return Err(TopicSettingsError::BackendsDisagree {
                        first: first.clone(),
                        second: key.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        Ok(chosen.map_or_else(
            || BackendSettings {
                r#type: self.default_storage_backend.clone(),
                settings: serde_json::Map::new(),
            },
            |(_, block)| BackendSettings {
                r#type: block.r#type.clone(),
                settings: block.settings.clone(),
            },
        ))
    }
}

impl EventBrokerConfig {
    /// The two entries that can reach one topic: its own, and the
    /// instance-less entry for its type. Either may be absent.
    ///
    /// Exposed rather than folded here because the fold has a third tier this
    /// type cannot see - what the topic's own specification declares - and a
    /// ladder split across two files would be two places to get the order
    /// wrong. `crate::domain::resolution` applies all of them.
    ///
    /// # Errors
    /// [`TopicSettingsError::MalformedTopicId`] when the identifier does not
    /// decompose into a type and an instance part, so no instance-less key
    /// could be derived from it.
    pub fn entries_for(
        &self,
        topic: &toolkit_gts::GtsInstanceId,
    ) -> Result<(Option<&TopicSettingsEntry>, Option<&TopicSettingsEntry>), TopicSettingsError>
    {
        let topic_id = topic.as_ref();
        let type_id = type_id_of(topic_id)?;
        Ok((
            self.topics.entries.get(topic_id),
            self.topics.entries.get(&type_id),
        ))
    }
}

/// The instance-less key for a topic identifier: everything up to and
/// including the type's trailing `~`.
fn type_id_of(topic_id: &str) -> Result<String, TopicSettingsError> {
    let parsed =
        gts::GtsId::try_new(topic_id).map_err(|e| TopicSettingsError::MalformedTopicId {
            topic: topic_id.to_owned(),
            reason: e.to_string(),
        })?;
    parsed
        .get_type_id()
        .ok_or_else(|| TopicSettingsError::MalformedTopicId {
            topic: topic_id.to_owned(),
            reason: "identifier carries no type segment to key a default on".to_owned(),
        })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
