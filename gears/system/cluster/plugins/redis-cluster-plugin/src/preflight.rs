//! Startup preflight (DESIGN.md §3.4) and the consistency declaration
//! (DESIGN.md §3.6): the reply parsers, and the decision table as a pure
//! function.
//!
//! ## The decision is pure, the detection is not
//!
//! [`decide_consistency`] takes only what the preflight learned — a topology
//! finding, the operator's durability hint, and what `CONFIG GET` said about
//! durability — and returns the declaration. It issues no command, holds no
//! client, and logs nothing. That split is deliberate: the consistency
//! declaration is the single most consequential thing this plugin decides
//! (it is what makes a Redis-backed profile fail or start, ADR-009), and every
//! row of DESIGN.md §3.6's table plus the hint-contradiction case is therefore
//! exercisable as a unit test with no server, rather than only against whatever
//! topologies a container fixture happens to reproduce.
//!
//! The `fred`-dependent half is [`run_preflight`]: it issues `INFO` and
//! `CONFIG GET`, degrades when a managed Redis refuses either, and logs the
//! reason for every conservative fallback. It feeds this module's parsers and
//! then this module's decision, and holds no policy of its own — every branch
//! it takes on a *value* is one of the pure functions above.
//!
//! ## Nothing here upgrades a guarantee it did not verify
//!
//! Every unreadable answer resolves to the *weaker* declaration, and the one
//! case where an unverifiable operator hint is trusted — a `fsync_always` claim
//! the plugin cannot check because `CONFIG` is refused — is reported back
//! through [`ConsistencyDecision::asserted_not_verified`] so the caller can log
//! `cluster.provider.consistency_asserted`. An operator's claim is then visible
//! in the logs of the deployment that made it.

use std::collections::BTreeMap;

use cluster_sdk::{CacheConsistency, ClusterError, ProviderErrorKind};
use fred::interfaces::{ClientLike, ConfigInterface};
use fred::types::InfoKind;
use tracing::{debug, warn};

use crate::config::{Durability, Topology};
use crate::observability::logs;
use crate::redis_error::map_redis_error;

/// The `notify-keyspace-events` flags the cache's watch needs (DESIGN.md §4.3).
///
/// `K` turns on keyspace notifications, `x` adds `expired`, and `e` adds
/// `evicted` — the two events no plugin code can publish for itself
/// (DESIGN.md §4.3).
///
/// Minimal matters here rather than being tidiness. Every flag is
/// **server-wide**, and `manage_keyspace_notifications: true` writes the setting
/// globally, so anything beyond these three is traffic unrelated tenants of the
/// same Redis pay for and this plugin never reads. The tempting additions are the
/// worst offenders: `g` adds a notification for every generic command and `$` for
/// every string command.
pub const REQUIRED_KEYSPACE_FLAGS: &str = "Kxe";

/// The subset [`EVICTION_KEYSPACE_FLAGS`] needs: `K` to route the notification
/// to a `__keyspace@…__` channel, `e` for `evicted` itself.
///
/// Separate from [`REQUIRED_KEYSPACE_FLAGS`] because the two deployments want
/// different things and asking for the union would overstate one of them. The
/// cache's watch needs `x` as well, since an entry whose TTL lapses has no other
/// way to reach a watcher. The eviction signal of DESIGN.md §3.7 needs no `x` at
/// all: a lapsed **lock lease** is not an incident, and the standalone lock
/// plugin (DESIGN.md §3.5) would be asking a shared server for a server-wide
/// flag it never reads.
pub const EVICTION_KEYSPACE_FLAGS: &str = "Ke";

/// The first Redis major version with sharded pub/sub (`SPUBLISH`/`SSUBSCRIBE`).
///
/// Detected and recorded but never used: v1 publishes with plain `PUBLISH`,
/// which is broadcast cluster-wide and therefore correct on every topology,
/// and DESIGN.md §13 D3 keeps the sharded variant as a follow-up. Knowing
/// whether a deployment *could* take it is an input to that decision, which is
/// why the finding is a DEBUG line rather than an INFO one.
pub const SHARDED_PUBSUB_MAJOR: u32 = 7;

/// The `maxmemory-policy` value under which cluster keys cannot be evicted.
///
/// Any other policy is a documented misconfiguration (DESIGN.md §3.7): an
/// evicted lock key hands the lock to a second holder while the first still
/// believes it holds it, and no TTL has lapsed and no consumer is told.
pub const SAFE_MAXMEMORY_POLICY: &str = "noeviction";

/// The role a server reports in `INFO replication`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationRole {
    /// `role:master` — accepts writes.
    Primary,
    /// `role:slave` — a replica, and therefore by definition part of a
    /// replicated topology.
    Replica,
}

/// The two `INFO replication` fields the topology decision reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicationInfo {
    /// The reported `role`.
    pub role: ReplicationRole,
    /// The reported `connected_slaves`, or `None` when the field was absent.
    ///
    /// Absent is not the same as zero, and the difference is load-bearing:
    /// only a primary that *reports* zero replicas can reach the single-node
    /// row of DESIGN.md §3.6, so a reply that omits the field is treated as
    /// replicated rather than assumed quiet.
    pub connected_replicas: Option<u32>,
}

/// The topology input to DESIGN.md §3.6's table.
///
/// The single-node case is split by provenance and the others are not, because
/// only the single-node row can produce `Linearizable`: for every other row the
/// answer is `EventuallyConsistent` whether the plugin read it off the server
/// or took the operator's word for it, so carrying the distinction there would
/// be a variant nothing branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyFinding {
    /// `INFO replication` reported `role:master` with `connected_slaves:0`.
    VerifiedSingleNode,
    /// The operator's `topology: standalone` hint, with detection skipped
    /// (DESIGN.md §3.4). Trusted, but never verified — which is what makes a
    /// `Linearizable` declaration built on it asserted rather than confirmed.
    AssertedSingleNode,
    /// Sentinel, a primary with replicas, or a replica. Asynchronous
    /// replication means every failover may promote a node that never saw an
    /// accepted write.
    Replicated,
    /// Redis Cluster: the same asynchronous replication, plus slot-migration
    /// edge cases.
    Cluster,
    /// `INFO replication` was refused or unintelligible and no operator hint
    /// was given.
    ///
    /// Distinct from [`Self::Replicated`] even though both decide the same way.
    /// DESIGN.md §3.4 says an unreadable `INFO replication` is treated *as*
    /// replicated, and it is — but the caller logs a different reason for each,
    /// and an operator debugging a surprise `EventuallyConsistent` needs to
    /// know whether the plugin saw a replica or saw nothing.
    Unknown,
}

/// The `appendfsync` policy, as the server reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appendfsync {
    /// Every write is fsynced before it is acknowledged.
    Always,
    /// The Redis default: fsync once a second, so a crash can lose up to a
    /// second of already-acknowledged writes.
    Everysec,
    /// Leave fsync to the operating system.
    No,
}

impl Appendfsync {
    /// Parses the `appendfsync` value Redis reports, or `None` for a value this
    /// plugin does not recognize.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "always" => Some(Self::Always),
            "everysec" => Some(Self::Everysec),
            "no" => Some(Self::No),
            _ => None,
        }
    }
}

/// What `CONFIG GET appendonly|appendfsync` reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityReading {
    /// Both settings were read back.
    Readable {
        /// `appendonly yes`.
        appendonly: bool,
        /// The `appendfsync` policy.
        appendfsync: Appendfsync,
    },
    /// `CONFIG GET` was refused, unavailable, or answered with something this
    /// plugin could not parse.
    ///
    /// Managed Redis (`ElastiCache`, `MemoryDB`, Azure Cache) commonly restricts
    /// `CONFIG`, so this is an ordinary state rather than a failure — the
    /// plugin treats it as non-durable and carries on (DESIGN.md §3.4).
    Unreadable,
}

impl DurabilityReading {
    /// Whether the server's own answer is the one configuration ADR-009 rates
    /// safe: an append-only file fsynced before every acknowledgement.
    #[must_use]
    fn is_fsync_always(self) -> bool {
        matches!(
            self,
            Self::Readable {
                appendonly: true,
                appendfsync: Appendfsync::Always,
            }
        )
    }

    /// Renders the reading the way the contradiction error reports it, so the
    /// operator sees the two `CONFIG` values rather than a verdict.
    fn describe(self) -> String {
        match self {
            Self::Readable {
                appendonly,
                appendfsync,
            } => {
                let appendonly = if appendonly { "yes" } else { "no" };
                let appendfsync = match appendfsync {
                    Appendfsync::Always => "always",
                    Appendfsync::Everysec => "everysec",
                    Appendfsync::No => "no",
                };
                format!("appendonly {appendonly}, appendfsync {appendfsync}")
            }
            Self::Unreadable => "unreadable".to_owned(),
        }
    }
}

/// The outcome of DESIGN.md §3.6's decision table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsistencyDecision {
    /// What `ClusterCacheBackend::consistency()` will return for the life of
    /// the plugin. Computed once at startup and never re-evaluated — a
    /// topology that changes underneath a running plugin does not downgrade a
    /// live declaration, which is a real gap recorded in DESIGN.md §12 rather
    /// than solved, because the resolution model has no way to express a
    /// backend whose capabilities change after consumers resolved against them.
    pub consistency: CacheConsistency,

    /// `true` when a [`CacheConsistency::Linearizable`] declaration rests on an
    /// operator hint the preflight could not check, so the caller should log
    /// `cluster.provider.consistency_asserted` (WARN, once, DESIGN.md §3.6).
    ///
    /// Always `false` for an `EventuallyConsistent` declaration: the flag warns
    /// about an unverified *upgrade*, and there is no such thing as an
    /// unverified downgrade — the conservative answer needs no evidence.
    pub asserted_not_verified: bool,
}

/// Resolves the topology the consistency decision branches on, preferring the
/// operator's hint over detection.
///
/// The hint wins outright rather than being cross-checked, unlike the
/// durability hint below. That asymmetry is DESIGN.md §3.4's: the hint exists
/// for a locked-down managed instance whose `INFO` the plugin cannot read at
/// all, so there is usually nothing to cross-check against, and unlike
/// `fsync_always` a topology claim is not the lever that unlocks
/// `Linearizable` on its own.
#[must_use]
pub fn resolve_topology(hint: Option<Topology>, detected: TopologyFinding) -> TopologyFinding {
    match hint {
        Some(Topology::Standalone) => TopologyFinding::AssertedSingleNode,
        Some(Topology::Sentinel) => TopologyFinding::Replicated,
        Some(Topology::Cluster) => TopologyFinding::Cluster,
        None => detected,
    }
}

/// Derives the topology finding from a parsed `INFO replication`, or
/// [`TopologyFinding::Unknown`] when the reply could not be parsed.
///
/// Only `role:master` with an explicit `connected_slaves:0` yields
/// [`TopologyFinding::VerifiedSingleNode`]. A replica is replicated by
/// definition, a primary with replicas obviously so, and a primary whose reply
/// omitted the replica count is treated as replicated because the single-node
/// row is the only one that can weaken a guarantee by being wrong.
///
/// Cluster mode is not derivable from `INFO replication` — every node in a
/// cluster reports a plain `role` — so it comes from the operator hint or from
/// the connection URL scheme, both handled by [`resolve_topology`] and its
/// caller.
#[must_use]
pub fn topology_from_replication(info: Option<ReplicationInfo>) -> TopologyFinding {
    match info {
        Some(ReplicationInfo {
            role: ReplicationRole::Primary,
            connected_replicas: Some(0),
        }) => TopologyFinding::VerifiedSingleNode,
        Some(_) => TopologyFinding::Replicated,
        None => TopologyFinding::Unknown,
    }
}

/// DESIGN.md §3.6's decision table, as a pure function of the topology finding,
/// the operator's `durability` hint, and what `CONFIG GET` reported.
///
/// | Topology | Durable writes | Declaration |
/// |---|---|---|
/// | Single node, verified or asserted | `appendonly yes` + `appendfsync always` | `Linearizable` |
/// | Single node | anything weaker | `EventuallyConsistent` |
/// | Sentinel / any replicated primary | — | `EventuallyConsistent` |
/// | Redis Cluster | — | `EventuallyConsistent` |
/// | Unknown | — | `EventuallyConsistent` |
///
/// `WAIT` does not appear because it changes nothing here: per ADR-009's "no
/// linearizable-ish middle ground", `WAIT 1` narrows the Sentinel failover
/// window but does not close it, and `CacheConsistency` is deliberately
/// two-valued.
///
/// # Errors
/// [`ClusterError::InvalidConfig`] when the operator declared
/// `durability: fsync_always` and a readable `CONFIG GET` contradicts it. The
/// error names both the claim and the two values the server reported, because
/// the operator has to change one of them and cannot tell which from a verdict
/// alone. Only this direction fails: a hint *weaker* than the server's actual
/// setting can only under-declare, which is safe, and is a legitimate choice
/// for an operator who does not want a durable-today server to silently become
/// the basis of a `Linearizable` declaration tomorrow.
pub fn decide_consistency(
    topology: TopologyFinding,
    durability: Option<Durability>,
    readability: DurabilityReading,
) -> Result<ConsistencyDecision, ClusterError> {
    let claims_fsync_always = durability == Some(Durability::FsyncAlways);

    if claims_fsync_always
        && matches!(readability, DurabilityReading::Readable { .. })
        && !readability.is_fsync_always()
    {
        return Err(ClusterError::InvalidConfig {
            reason: format!(
                "durability: fsync_always was declared for this Redis binding, but the server \
                 reports {}. Either set appendonly yes with appendfsync always on the server, or \
                 remove the durability hint so the plugin declares the consistency it can verify \
                 (DESIGN.md sec 3.6)",
                readability.describe()
            ),
        });
    }

    // The hint, when present, decides; only `fsync_always` can mean durable, so
    // an `everysec` or `none` hint settles the question without consulting the
    // server at all. With no hint, the server's answer decides, and an
    // unreadable answer is non-durable — the conservative direction.
    let durable_writes = match durability {
        Some(Durability::FsyncAlways) => true,
        Some(Durability::FsyncEverysec | Durability::None) => false,
        None => readability.is_fsync_always(),
    };

    let single_node = matches!(
        topology,
        TopologyFinding::VerifiedSingleNode | TopologyFinding::AssertedSingleNode
    );

    if single_node && durable_writes {
        // Asserted whenever either leg rests on something unchecked: a
        // `topology: standalone` hint (no `INFO replication` was read, so the
        // "no replicas" half is the operator's word) or a `fsync_always` hint
        // that `CONFIG GET` could not corroborate.
        let topology_verified = topology == TopologyFinding::VerifiedSingleNode;
        let durability_verified = readability.is_fsync_always();
        return Ok(ConsistencyDecision {
            consistency: CacheConsistency::Linearizable,
            asserted_not_verified: !(topology_verified && durability_verified),
        });
    }

    Ok(ConsistencyDecision {
        consistency: CacheConsistency::EventuallyConsistent,
        asserted_not_verified: false,
    })
}

/// Parses an `INFO` reply into its `key:value` pairs.
///
/// Shared by the `INFO server` and `INFO replication` checks of DESIGN.md §3.4
/// rather than written once per section, since the reply format is the same for
/// both: `# Section` header lines, blank lines, and `key:value` lines with
/// `\r\n` endings. The split is on the **first** colon only, because values
/// legitimately contain them (`slave0:ip=10.0.0.1,port=6379,…`).
#[must_use]
pub fn parse_info(reply: &str) -> BTreeMap<&str, &str> {
    reply
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once(':'))
        .collect()
}

/// Extracts the topology-relevant fields from an `INFO replication` reply, or
/// `None` when the reply carries no recognizable `role`.
///
/// A missing or unrecognized `role` is the "unreadable" case rather than an
/// error: DESIGN.md §3.4 degrades on it instead of failing startup, because a
/// managed Redis may restrict `INFO` sections and refusing to run there would
/// make the plugin unusable on the platforms it is most often deployed to.
#[must_use]
pub fn parse_info_replication(reply: &str) -> Option<ReplicationInfo> {
    let fields = parse_info(reply);
    let role = match fields.get("role")?.trim() {
        "master" => ReplicationRole::Primary,
        "slave" => ReplicationRole::Replica,
        _ => return None,
    };
    let connected_replicas = fields
        .get("connected_slaves")
        .and_then(|raw| raw.trim().parse::<u32>().ok());
    Some(ReplicationInfo {
        role,
        connected_replicas,
    })
}

/// Parses a `CONFIG GET` reply — a flat array of alternating parameter names
/// and values — into a map.
///
/// The empty-value case is ordinary rather than exceptional and must survive
/// the round trip: an unconfigured `notify-keyspace-events` reads back as
/// `["notify-keyspace-events", ""]`, and collapsing that to "absent" would make
/// the plugin unable to distinguish "the server has no flags set" from "the
/// server would not tell me", which are opposite conclusions
/// ([`missing_keyspace_flags`] versus a WARN and best-effort `Expired`).
///
/// # Errors
/// [`ClusterError::Provider`] with [`ProviderErrorKind::Other`] when the reply
/// has an odd number of elements. That is the server or the client library
/// violating the command's own reply shape, and silently dropping the unpaired
/// trailing element would turn it into a missing setting.
pub fn parse_config_get(reply: &[String]) -> Result<BTreeMap<String, String>, ClusterError> {
    if !reply.len().is_multiple_of(2) {
        return Err(ClusterError::Provider {
            kind: ProviderErrorKind::Other,
            message: format!(
                "CONFIG GET returned {} elements; a parameter/value reply must have an even count",
                reply.len()
            ),
        });
    }
    Ok(reply
        .chunks_exact(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect())
}

/// Returns the flags of `required` that `current` — the server's
/// `notify-keyspace-events` value — does not already provide, in the order they
/// are listed.
///
/// `required` is [`REQUIRED_KEYSPACE_FLAGS`] or [`EVICTION_KEYSPACE_FLAGS`]
/// depending on which plugin is asking, rather than a constant read from here:
/// a lock-only deployment must not be told it is missing `x`.
///
/// `A` is handled rather than compared literally: Redis defines it as an alias
/// for every event class except `K`, `E`, `m`, and `n`, so a server configured
/// `KA` already delivers `expired` and `evicted` and needs nothing added. It
/// does *not* imply `K`, which is what actually routes those events to a
/// `__keyspace@…__` channel, so `K` is still required alongside it.
#[must_use]
pub fn missing_keyspace_flags(current: &str, required: &str) -> Vec<char> {
    let has_all_classes = current.contains('A');
    required
        .chars()
        .filter(|flag| {
            let covered_by_alias = has_all_classes && *flag != 'K';
            !current.contains(*flag) && !covered_by_alias
        })
        .collect()
}

/// The value to pass to `CONFIG SET notify-keyspace-events` so the server keeps
/// everything it already emits and gains what this plugin needs — the one
/// mutation `manage_keyspace_notifications: true` permits (DESIGN.md §3.4).
///
/// Additive by construction. The setting is server-wide, so replacing it with
/// `required` alone would silently switch off notifications an unrelated tenant
/// of the same Redis is subscribed to.
#[must_use]
pub fn merge_keyspace_flags(current: &str, required: &str) -> String {
    let mut merged = current.to_owned();
    merged.extend(missing_keyspace_flags(current, required));
    merged
}

/// Whether `version` — the `redis_version` field of `INFO server` — is new
/// enough for sharded pub/sub (DESIGN.md §13 D3).
///
/// Only the major component is read, and an unparseable one answers `false`.
/// The conservative direction is the right one here for an unusual reason: the
/// answer feeds a DEBUG line and nothing else, so a false negative costs a log
/// line while a false positive would put a wrong capability claim in the record
/// a future decision is made from.
#[must_use]
pub fn supports_sharded_pubsub(version: &str) -> bool {
    version
        .trim()
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= SHARDED_PUBSUB_MAJOR)
}

/// Whether `policy` — the server's `maxmemory-policy` — leaves cluster keys
/// safe from eviction (DESIGN.md §3.7).
///
/// A `false` here is a WARN and not a startup failure, because the policy is a
/// server-wide setting the cluster keys may be sharing with unrelated tenants:
/// refusing to start would make this plugin unusable on any shared Redis, which
/// is a worse outcome than an alertable warning on a real risk.
#[must_use]
pub fn maxmemory_policy_is_safe(policy: &str) -> bool {
    policy.trim().eq_ignore_ascii_case(SAFE_MAXMEMORY_POLICY)
}

/// What the caller wants the preflight to check, and the operator hints it must
/// honour (DESIGN.md §3.4).
#[derive(Debug, Clone, Copy)]
pub struct PreflightRequest {
    /// The operator's `topology` hint. When set, `INFO replication` detection is
    /// skipped entirely.
    pub topology_hint: Option<Topology>,
    /// What the connection URL itself says about the topology — `Cluster` for a
    /// `redis-cluster://` URL, `Sentinel` for `redis-sentinel://`, `None`
    /// otherwise.
    ///
    /// A fact about the client rather than a claim about the server, so it
    /// outranks detection: a clustered client is talking to a cluster whatever a
    /// single node's `INFO replication` reports about its own replicas. It still
    /// loses to an explicit operator hint, which is the documented override for
    /// everything the plugin infers.
    pub url_topology: Option<Topology>,
    /// The operator's `durability` hint, cross-checked against `CONFIG GET`
    /// whenever that is readable.
    pub durability_hint: Option<Durability>,
    /// Which `notify-keyspace-events` flags this deployment needs, or `None` to
    /// skip the check entirely.
    ///
    /// [`REQUIRED_KEYSPACE_FLAGS`] for the combined plugin, whose cache watch
    /// needs `expired` as well; [`EVICTION_KEYSPACE_FLAGS`] for the standalone
    /// lock plugin, which needs only the eviction signal of DESIGN.md §3.7.
    ///
    /// What the two deployments lose without their flags differs in kind, which
    /// is why the set is a request field rather than a constant. The cache loses
    /// prompt `Expired` delivery. The lock loses **nothing it needs to operate**
    /// — a lease lapse is still discovered by the next acquire attempt — and
    /// only forfeits the report that an eviction handed one of its locks to a
    /// second holder. Neither is fatal; both are worth a WARN.
    pub keyspace_flags: Option<&'static str>,
    /// Whether the plugin may issue one `CONFIG SET notify-keyspace-events` to
    /// add the flags it needs (DESIGN.md §3.4).
    pub manage_keyspace_notifications: bool,
}

/// What the preflight concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreflightOutcome {
    /// The declaration `consistency()` returns for the life of the handle.
    pub consistency: CacheConsistency,
}

/// Runs the DESIGN.md §3.4 preflight against a connected client and computes the
/// §3.6 consistency declaration — step 2 of `build_and_start` (DESIGN.md §3.2).
///
/// Generic over the client rather than taking a `Pool` so the standalone lock
/// plugin can run the same checks against its own pool, and so nothing here
/// depends on which of `fred`'s client types is in play.
///
/// **Every check except the first degrades rather than fails.** Managed Redis
/// commonly restricts `CONFIG`, and a plugin that treated an ACL denial as a
/// hard failure would be unusable on the platforms this backend is most often
/// deployed to. `INFO server` is the exception: a server that will not answer it
/// at all is locked down past the point of usefulness, and failing at startup is
/// better than discovering that one command at a time.
///
/// # Errors
/// - [`ClusterError::InvalidConfig`] when `INFO server` is refused, or when a
///   `durability: fsync_always` hint is contradicted by a readable server config
///   (via [`decide_consistency`]).
/// - [`ClusterError::Provider`] for a `CONFIG SET` that fails while
///   `manage_keyspace_notifications` is enabled — the operator asked for the
///   write, so it is not silently degraded.
pub async fn run_preflight<C>(
    client: &C,
    request: PreflightRequest,
) -> Result<PreflightOutcome, ClusterError>
where
    C: ClientLike + ConfigInterface,
{
    require_reachable_server(client).await?;
    let consistency = declare_consistency(client, request).await?;
    warn_on_eviction_policy(client).await;

    // Run for its `?` and its WARN, not for its answer. Nothing branches on
    // keyspace availability: the degradation is fully reported by the WARN
    // `ensure_keyspace_notifications` emits where it discovers it, and a field
    // holding the answer would be state the plugin maintains and never consults
    // — the shape DESIGN.md §13 D3 rejects by name, where it also says the
    // change that first *uses* it should decide where it lives.
    if let Some(required) = request.keyspace_flags {
        let _available =
            ensure_keyspace_notifications(client, required, request.manage_keyspace_notifications)
                .await?;
    }

    Ok(PreflightOutcome { consistency })
}

/// The one preflight check that fails rather than degrades (DESIGN.md §3.4).
///
/// # Errors
/// [`ClusterError::InvalidConfig`] when `INFO server` is refused.
async fn require_reachable_server<C: ClientLike>(client: &C) -> Result<(), ClusterError> {
    let info: String =
        client
            .info(Some(InfoKind::Server))
            .await
            .map_err(|err| ClusterError::InvalidConfig {
                reason: format!(
                    "redis: `INFO server` was refused ({err}); a server this locked down cannot be \
                 preflighted, so the plugin will not start against it (DESIGN.md sec 3.4)"
                ),
            })?;
    let Some(version) = parse_info(&info).get("redis_version").copied() else {
        return Ok(());
    };
    debug!(redis_version = version, "redis preflight: server reached");
    if supports_sharded_pubsub(version) {
        debug!(
            name: logs::SHARDED_PUBSUB_AVAILABLE,
            redis_version = version,
            "cluster.provider.sharded_pubsub_available: this server supports SPUBLISH/SSUBSCRIBE, \
             which v1 records and does not use. Plain PUBLISH is broadcast cluster-wide and is \
             therefore correct on every topology; the sharded variant is a follow-up \
             (DESIGN.md sec 13 D3)"
        );
    }
    Ok(())
}

/// Resolves the topology, reads the durability, and runs the §3.6 table over
/// both — warning when the answer rests on an operator hint the plugin could not
/// check, and warning again whenever the answer is the weak one.
///
/// The two warnings are not alternatives. `consistency_asserted` says *the
/// evidence was missing*; `weak_consistency` says *the declaration is
/// `EventuallyConsistent`*, which is the expected state for Sentinel and Cluster
/// and still has to appear in the log of every deployment it applies to
/// (DESIGN.md §9). Exactly one of them can fire per startup, because an asserted
/// declaration is by construction a `Linearizable` one.
///
/// # Errors
/// [`ClusterError::InvalidConfig`] when a `durability: fsync_always` hint is
/// contradicted by a readable server config.
async fn declare_consistency<C>(
    client: &C,
    request: PreflightRequest,
) -> Result<CacheConsistency, ClusterError>
where
    C: ClientLike + ConfigInterface,
{
    // The operator's hint outranks the URL, which outranks detection — each
    // layer is more specific about intent than the one below it.
    //
    // Detection runs only when nothing above it has already settled the answer.
    // `resolve_topology` returns a constant from every `Some(_)` arm, so with an
    // assertion in hand the `INFO replication` round trip decides nothing — and
    // issuing it anyway costs more than a wasted command. The reason to set
    // `topology: standalone` on a locked-down managed instance is precisely that
    // `INFO replication` is refused there, so the refusal would log
    // `cluster.provider.topology_unknown` announcing a conservative
    // `EventuallyConsistent` declaration that `resolve_topology` is not going to
    // make. Skipping the call is what both `PreflightRequest::topology_hint` and
    // `config::Topology` already promise, and it is what makes that WARN
    // truthful wherever it does fire.
    //
    // The URL-derived topology settles it just as an explicit hint does: a
    // `redis-cluster://` URL is a fact about the client rather than a claim
    // about the server (see [`PreflightRequest::url_topology`]).
    let asserted = request.topology_hint.or(request.url_topology);
    let topology = match asserted {
        Some(hint) => resolve_topology(Some(hint), TopologyFinding::Unknown),
        None => resolve_topology(None, detect_topology(client).await),
    };
    let durability = read_durability(client).await;
    let decision = decide_consistency(topology, request.durability_hint, durability)?;
    if decision.asserted_not_verified {
        warn!(
            name: logs::CONSISTENCY_ASSERTED,
            topology = ?topology,
            "cluster.provider.consistency_asserted: declaring Linearizable on the strength of an \
             operator hint this server would not let the plugin verify. If the hint is wrong, \
             leader election over this cache can elect two leaders (DESIGN.md sec 3.6)"
        );
    }
    if decision.consistency == CacheConsistency::EventuallyConsistent {
        warn!(
            name: logs::WEAK_CONSISTENCY,
            topology = ?topology,
            durability = ?durability,
            "cluster.provider.weak_consistency: this Redis is declared EventuallyConsistent, so \
             ADR-009 rates it unsafe for CAS-based leader election and for the SDK's default \
             lock: an accepted write can be lost to an fsync gap or a failover, which is two \
             leaders. Not an error and the expected state for Sentinel and Cluster, but a profile \
             binding this cache must route leader election elsewhere or opt in explicitly \
             (DESIGN.md sec 3.6, sec 7)"
        );
    }
    Ok(decision.consistency)
}

/// `INFO replication`, degrading to [`TopologyFinding::Unknown`] when the server
/// will not answer.
///
/// DESIGN.md §3.4 says an unreadable `INFO replication` is treated as
/// replicated; `Unknown` is that, plus the ability to say *why* in the log.
async fn detect_topology<C: ClientLike>(client: &C) -> TopologyFinding {
    match client.info::<String>(Some(InfoKind::Replication)).await {
        Ok(reply) => topology_from_replication(parse_info_replication(&reply)),
        Err(err) => {
            warn!(
                name: logs::TOPOLOGY_UNKNOWN,
                error = %err,
                "cluster.provider.topology_unknown: `INFO replication` was refused, so the \
                 topology could not be detected; declaring the conservative \
                 EventuallyConsistent (DESIGN.md sec 3.4)"
            );
            TopologyFinding::Unknown
        }
    }
}

/// `CONFIG GET appendonly` and `CONFIG GET appendfsync`, degrading to
/// [`DurabilityReading::Unreadable`] if either is refused or unrecognized.
///
/// Both are read, not just `appendfsync`, because `appendfsync always` means
/// nothing with `appendonly no` — there is no append-only file to fsync, and a
/// check that read only the policy would declare `Linearizable` for a server
/// keeping no durable log at all.
async fn read_durability<C: ConfigInterface>(client: &C) -> DurabilityReading {
    let Some(appendonly) = read_config_value(client, "appendonly").await else {
        return DurabilityReading::Unreadable;
    };
    let Some(appendfsync) = read_config_value(client, "appendfsync").await else {
        return DurabilityReading::Unreadable;
    };
    let Some(appendfsync) = Appendfsync::parse(&appendfsync) else {
        warn!(
            name: logs::DURABILITY_UNKNOWN,
            value = appendfsync,
            "cluster.provider.durability_unknown: `appendfsync` reported a value this plugin does \
             not recognize; treating durability as unverifiable (DESIGN.md sec 3.4)"
        );
        return DurabilityReading::Unreadable;
    };
    DurabilityReading::Readable {
        appendonly: appendonly.trim().eq_ignore_ascii_case("yes"),
        appendfsync,
    }
}

/// One `CONFIG GET`, returning `None` when the command is refused or the
/// parameter is absent from the reply.
///
/// The reply is taken as the flat parameter/value array RESP2 defines and run
/// through [`parse_config_get`] rather than decoded straight into a map, so the
/// empty-value case stays distinguishable from an absent one — see that
/// function for why the difference matters.
async fn read_config_value<C: ConfigInterface>(client: &C, parameter: &str) -> Option<String> {
    let reply: Vec<String> = match client.config_get(parameter.to_owned()).await {
        Ok(reply) => reply,
        Err(err) => {
            debug!(
                parameter,
                error = %err,
                "redis preflight: CONFIG GET was refused; degrading for this check"
            );
            return None;
        }
    };
    // Not `.ok()?`: an odd-length reply is the server or `fred` violating the
    // reply shape, which `parse_config_get` documents must not be dropped
    // silently. Degrading to `None` is still the conservative answer, but
    // without this line it is indistinguishable from the refusal above and the
    // protocol violation leaves no trace anywhere.
    match parse_config_get(&reply) {
        Ok(parsed) => parsed.get(parameter).cloned(),
        Err(err) => {
            // not-a-catalogued-event: the server or `fred` breaking the RESP
            // reply shape is a defect in one of them, not a condition an
            // operator can act on — the check has already degraded safely. WARN
            // rather than the DEBUG the refusal arm uses, because a refusal is
            // expected on an ACL-restricted deployment and this is expected
            // nowhere.
            warn!(
                parameter,
                error = %err,
                "redis preflight: CONFIG GET returned a malformed parameter/value reply; \
                 degrading for this check"
            );
            None
        }
    }
}

/// Reads `maxmemory-policy` and warns if cluster keys can be evicted
/// (DESIGN.md §3.7).
///
/// WARN and never an error: the policy is server-wide and the cluster keys may
/// be sharing the instance with unrelated tenants, so refusing to start would
/// make this plugin unusable on any shared Redis — a worse outcome than an
/// alertable warning on a real risk.
async fn warn_on_eviction_policy<C: ConfigInterface>(client: &C) {
    let Some(policy) = read_config_value(client, "maxmemory-policy").await else {
        warn!(
            name: logs::MAXMEMORY_POLICY_UNKNOWN,
            "cluster.provider.maxmemory_policy_unknown: `CONFIG GET maxmemory-policy` was \
             refused, so the plugin cannot tell whether its keys can be evicted. An evicted lock \
             or leader key hands the name to a second holder with no TTL having lapsed \
             (DESIGN.md sec 3.7)"
        );
        return;
    };
    if !maxmemory_policy_is_safe(&policy) {
        warn!(
            name: logs::MAXMEMORY_POLICY_UNSAFE,
            policy,
            "cluster.provider.maxmemory_policy_unsafe: this Redis can evict the plugin's own \
             keys, which silently breaks every primitive at once: an evicted lock key hands the \
             lock to a second holder while the first still believes it holds it. Run cluster keys \
             on a dedicated instance, or set maxmemory-policy noeviction (DESIGN.md sec 3.7)"
        );
    }
}

/// Checks `notify-keyspace-events`, optionally adding the missing flags, and
/// reports whether `expired`/`evicted` notifications will arrive.
///
/// # Errors
/// [`ClusterError::Provider`] when `manage_keyspace_notifications` is on and the
/// `CONFIG SET` fails. An operator who opted into the write gets told it did not
/// happen rather than a silent downgrade to best-effort `Expired` events.
async fn ensure_keyspace_notifications<C: ConfigInterface>(
    client: &C,
    required: &str,
    manage: bool,
) -> Result<bool, ClusterError> {
    let Some(current) = read_config_value(client, "notify-keyspace-events").await else {
        warn!(
            name: logs::EXPIRY_EVENTS_UNAVAILABLE,
            required,
            "cluster.provider.expiry_events_unavailable: `notify-keyspace-events` could not be \
             read, so observed evictions cannot be reported, and where this deployment has a \
             cache its Expired events are best-effort. This degrades promptness and \
             observability, not correctness: an expired entry still reads as absent and a lapsed \
             lease is still found by the next acquire (DESIGN.md sec 4.3, sec 3.7)"
        );
        return Ok(false);
    };

    let missing = missing_keyspace_flags(&current, required);
    if missing.is_empty() {
        return Ok(true);
    }

    if !manage {
        warn!(
            name: logs::EXPIRY_EVENTS_UNAVAILABLE,
            current,
            required,
            missing = missing.iter().collect::<String>(),
            "cluster.provider.expiry_events_unavailable: this server's notify-keyspace-events \
             lacks flags this deployment needs. Add them on the server, or set \
             manage_keyspace_notifications: true to let the plugin add them (DESIGN.md sec 3.4)"
        );
        return Ok(false);
    }

    add_keyspace_flags(client, &current, required).await
}

/// Issues the one `CONFIG SET notify-keyspace-events` that
/// `manage_keyspace_notifications: true` permits, and confirms it took.
///
/// # Errors
/// [`ClusterError::Provider`] when the `CONFIG SET` itself fails.
async fn add_keyspace_flags<C: ConfigInterface>(
    client: &C,
    current: &str,
    required: &str,
) -> Result<bool, ClusterError> {
    // Additive, never a replacement: the setting is server-wide, so overwriting
    // it with just this plugin's flags would switch off notifications an
    // unrelated tenant of the same Redis is subscribed to.
    let merged = merge_keyspace_flags(current, required);
    client
        .config_set("notify-keyspace-events", merged.clone())
        .await
        .map_err(map_redis_error)?;

    // Re-read rather than trusting the write (DESIGN.md §3.4). A `CONFIG SET`
    // can succeed against a proxy that accepts and drops it, and the plugin
    // would then spend the deployment's life believing in events that never
    // arrive.
    let confirmed = read_config_value(client, "notify-keyspace-events")
        .await
        .is_some_and(|value| missing_keyspace_flags(&value, required).is_empty());
    if confirmed {
        // INFO rather than DEBUG, and this is the only config *mutation* the
        // plugin ever performs: `notify-keyspace-events` is server-wide, so an
        // operator reading this deployment's log has to be able to see that
        // their Redis was changed on a gear's behalf without turning DEBUG on
        // (DESIGN.md §9).
        tracing::info!(
            name: logs::KEYSPACE_NOTIFICATIONS_SET,
            previous = current,
            flags = merged,
            "cluster.provider.keyspace_notifications_set: manage_keyspace_notifications was \
             enabled and this server's global notify-keyspace-events has been extended, \
             additively, with the flags Expired and eviction events need (DESIGN.md sec 3.4)"
        );
    } else {
        warn!(
            name: logs::EXPIRY_EVENTS_UNAVAILABLE,
            requested = merged,
            "cluster.provider.expiry_events_unavailable: the CONFIG SET was accepted but the \
             flags did not stick on re-read; Expired events remain best-effort (DESIGN.md sec 3.4)"
        );
    }
    Ok(confirmed)
}

// Layer-1 unit tests (TESTING.md §2, `preflight.rs` row): all five rows of the
// §3.6 table, the hint-contradiction case, and the reply parsers. Out-of-line
// per DE1101.
#[cfg(test)]
#[path = "preflight_tests.rs"]
mod tests;
