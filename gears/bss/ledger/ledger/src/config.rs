//! Validated behavioral configuration for the Billing Ledger gear.
//!
//! Defines background-job cadences, seller tenant types, recognition, FX,
//! reconciliation, payment limits, and the event-publication gate. Database
//! selection is handled separately by the ToolKit `db` capability.

use std::time::Duration;

use serde::Deserialize;

// `Default` is derived now that every field's default is its type's default —
// the seller predicate deliberately starts empty (see `seller_tenant_types`),
// so there is nothing left for a hand-written impl to say.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BssLedgerConfig {
    /// Background-job cadences (tie-out, period-open). Defaults to daily.
    #[serde(default)]
    pub jobs: JobsConfig,
    /// ASC 606 revenue-recognition tunables (Slice 4): the per-schedule segment
    /// ceiling the `ScheduleBuilder` enforces and the recognition-run cadence.
    #[serde(default)]
    pub recognition: RecognitionConfig,
    /// FX & multi-currency tunables (Slice 5): rate-sync cadence, staleness
    /// thresholds, the deterministic provider order, and the Mode-B unrealized
    /// revaluation gate + cadence.
    #[serde(default)]
    pub fx: FxConfig,
    /// Reconciliation & period-close tunables (Slice 7 Phase 3): the
    /// `ReconciliationJob` cadence, the AR↔derived rounding tolerance, the
    /// manifest / bill-run close-enforcement gates (default OFF), and the
    /// close-drain lock timeout.
    #[serde(default)]
    pub recon: ReconConfig,
    /// Payments & allocation tunables (Slice 3): the per-allocation
    /// touched-invoice ceiling.
    #[serde(default)]
    pub payments: PaymentsConfig,
    /// Tenant types whose tenants own a billing ledger ("sellers").
    /// Provisioning rejects a target whose type is not matched here (the §4.12
    /// seller predicate, owned by the ledger — NOT an AM tenant-type trait,
    /// since GTS mandates closed trait schemas).
    ///
    /// **Required; there is deliberately no default.** Tenant-type identifiers
    /// are deployment-specific — a catalogue is registered by whoever operates
    /// the platform — so any built-in list would name one product and silently
    /// reject every seller on every other. An empty list is rejected by
    /// [`BssLedgerConfig::validate`] at `init()` rather than degrading into
    /// "no tenant owns a ledger" at the first provisioning request.
    ///
    /// Each entry is either a concrete chained type id
    /// (`gts.cf.core.am.tenant_type.v1~<vendor>.<pkg>.<ns>.<type>.v1~`) or a
    /// family wildcard ending in `*` (`gts.cf.core.am.tenant_type.v1~*` — every
    /// registered tenant type owns a ledger). Shapes are checked at `validate`;
    /// matching is [`seller_guard`](crate::infra::seller_guard).
    #[serde(default)]
    pub seller_tenant_types: Vec<String>,
    /// Register event-type schemas + build producers. Default OFF: bss-ledger is
    /// the platform's first event producer and the GTS event-type model is
    /// incomplete (`event.v1~<...>` schemas fail types-registry ready-commit —
    /// the schema doc validates as an INSTANCE of the event base, not a derived
    /// type). Re-enable once event-broker models event types correctly.
    #[serde(default)]
    pub events_enabled: bool,
}

/// Tick cadences for the gear's `RunnableCapability` background jobs.
///
/// Both default to once per day. The cadence only approximates the
/// fiscal-period boundary (no `chrono-tz`); the jobs themselves are
/// idempotent, so a coarse tick is harmless.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "the *_tick_secs suffix is the config-key naming convention"
)]
pub struct JobsConfig {
    /// Seconds between tie-out (self-reconciliation) ticks.
    pub tie_out_tick_secs: u64,
    /// Seconds between fiscal-period-open ticks.
    pub period_open_tick_secs: u64,
    /// Seconds between queued-allocation sweep ticks (the deferred-apply
    /// backstop, §4.7). Defaults to every 5 minutes — far tighter than the daily
    /// fiscal jobs, because a queued allocation should apply promptly once its
    /// settlement lands (drain-on-settle covers the common case; this sweep is the
    /// backstop, so a few minutes is the worst-case apply latency).
    pub queue_applier_tick_secs: u64,
    /// Seconds between aged-alarm ticks (the §6 `Warn`-severity scan for queued
    /// work / parked unallocated cash that has aged past a threshold). Defaults to
    /// hourly — tighter than the daily fiscal jobs so genuinely stuck work surfaces
    /// within an hour of crossing the (currently 24h) age threshold, but not as hot
    /// as the queue sweep (an aged item is, by definition, not time-critical).
    pub aged_alarm_tick_secs: u64,
    /// Seconds between chain-verifier ticks (re-walk every tenant's
    /// tamper-evidence hash chain). Defaults to once per day.
    pub verify_tick_secs: u64,
    /// How often the daily tie-out runs the FULL all-time fold as a drift
    /// backstop instead of the incremental (baseline + open-period) path
    /// (VHP-1843): every `N`th tick folds full, the rest go incremental. `0` (or
    /// `1`) means every tick folds full — the pre-VHP-1843 behaviour. The first
    /// tick after startup always folds full. Defaults to `7` (≈ weekly at the
    /// daily cadence) — closed periods are immutable, so the incremental path is
    /// authoritative and the full fold is paranoia against baseline drift.
    pub tieout_full_every_n: u64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            tie_out_tick_secs: 86_400,
            period_open_tick_secs: 86_400,
            queue_applier_tick_secs: 300,
            aged_alarm_tick_secs: 3_600,
            verify_tick_secs: 86_400,
            tieout_full_every_n: 7,
        }
    }
}

/// A bss-ledger configuration validation failure (boot-time): a field violated
/// its constraint (a zero tick cadence, an out-of-bound staleness window, …).
/// Carries the field + the constraint so `init()` fails loud with a precise,
/// matchable error rather than an opaque string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A field that must be strictly positive was `0`.
    #[error("config: {field} must be > 0")]
    MustBePositive { field: &'static str },
    /// A field exceeded its allowed maximum.
    #[error("config: {field} must be <= {max} ({reason})")]
    AboveMax {
        field: &'static str,
        max: u64,
        reason: &'static str,
    },
    /// A field fell below its allowed minimum (for bounds stricter than "> 0").
    #[error("config: {field} must be >= {min} ({reason})")]
    BelowMin {
        field: &'static str,
        min: u64,
        reason: &'static str,
    },
    /// A field carries deployment-specific identifiers, so the gear ships no
    /// default and cannot start until the deployment supplies one.
    #[error("config: {field} must be set ({reason})")]
    Required {
        field: &'static str,
        reason: &'static str,
    },
    /// An entry is neither a concrete GTS type id nor a family wildcard.
    #[error(
        "config: {field} entry {value:?} must be a GTS type id ending in `~` \
         or a family wildcard ending in `*`, and must start with `gts.`"
    )]
    InvalidGtsPattern { field: &'static str, value: String },
}

impl BssLedgerConfig {
    /// Validate the whole gear configuration: this struct's own required
    /// fields, then every sub-config. `init()` calls exactly this, so a
    /// sub-config added later cannot be left unvalidated by omission.
    ///
    /// # Errors
    /// * [`ConfigError::Required`] — `seller_tenant_types` is empty. The gear
    ///   ships no default because tenant-type identifiers belong to the
    ///   deployment's catalogue; without the list every provisioning request
    ///   would be rejected as "not a ledger owner", which reads like a bug
    ///   rather than a missing setting.
    /// * [`ConfigError::InvalidGtsPattern`] — an entry is neither a concrete
    ///   GTS type id (`…~`) nor a family wildcard (`…*`). Entries are matched
    ///   literally at provisioning time and nothing else validates them, so a
    ///   typo would otherwise surface only as a rejected seller.
    /// * whatever the sub-config validators return.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.seller_tenant_types.is_empty() {
            return Err(ConfigError::Required {
                field: "seller_tenant_types",
                reason: "tenant-type ids are deployment-specific; \
                         see the gear README for the expected shape",
            });
        }
        for entry in &self.seller_tenant_types {
            validate_tenant_type_pattern("seller_tenant_types", entry)?;
        }

        self.jobs.validate()?;
        self.recognition.validate()?;
        self.fx.validate()?;
        self.recon.validate()?;
        self.payments.validate()
    }
}

/// Accept a concrete chained GTS **type** id (`gts.….v1~`) or a family
/// wildcard (`gts.….*`), delegating the grammar to the platform parser
/// (`GtsOps::parse_id`, which handles both forms) rather than re-deriving it
/// from string suffixes here.
///
/// Two rules on top of "it parses":
/// * it must be anchored under the GTS prefix, so a stray plain string cannot
///   pass as an identifier;
/// * a wildcard must keep at least one segment before `*`. The bare form
///   ("every type of every vendor owns a ledger") is never a deliberate seller
///   predicate — the same one-segment floor RBAC enforces on permission
///   targets.
fn validate_tenant_type_pattern(field: &'static str, entry: &str) -> Result<(), ConfigError> {
    let invalid = || ConfigError::InvalidGtsPattern {
        field,
        value: entry.to_owned(),
    };

    let Some(body) = entry.strip_prefix(toolkit_gts::GTS_ID_PREFIX) else {
        return Err(invalid());
    };
    let parsed = gts::GtsOps::parse_id(entry);
    if !parsed.ok {
        return Err(invalid());
    }
    if parsed.is_wildcard {
        // `body` is what follows the prefix; a lone `*` is the bare wildcard.
        if body.trim_end_matches('*').is_empty() {
            return Err(invalid());
        }
        return Ok(());
    }
    // A concrete entry must be a TYPE id (trailing `~`): tenant types are
    // registered as type schemas, and an instance id here would never match.
    if parsed.is_type == Some(true) {
        return Ok(());
    }
    Err(invalid())
}

impl JobsConfig {
    /// Validate the tick cadences.
    ///
    /// # Errors
    /// Returns `Err` if either cadence is `0`: `tokio::time::interval`
    /// panics on `Duration::ZERO`, so a zero tick would abort the serve
    /// task at runtime instead of failing loudly at `init()`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.tie_out_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "jobs.tie_out_tick_secs",
            });
        }
        if self.period_open_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "jobs.period_open_tick_secs",
            });
        }
        if self.queue_applier_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "jobs.queue_applier_tick_secs",
            });
        }
        if self.aged_alarm_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "jobs.aged_alarm_tick_secs",
            });
        }
        if self.verify_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "jobs.verify_tick_secs",
            });
        }
        Ok(())
    }

    /// Tie-out tick cadence as a [`Duration`].
    #[must_use]
    pub fn tie_out_interval(&self) -> Duration {
        Duration::from_secs(self.tie_out_tick_secs)
    }

    /// Period-open tick cadence as a [`Duration`].
    #[must_use]
    pub fn period_open_interval(&self) -> Duration {
        Duration::from_secs(self.period_open_tick_secs)
    }

    /// Queued-allocation sweep tick cadence as a [`Duration`].
    #[must_use]
    pub fn queue_applier_interval(&self) -> Duration {
        Duration::from_secs(self.queue_applier_tick_secs)
    }

    /// Aged-alarm tick cadence as a [`Duration`].
    #[must_use]
    pub fn aged_alarm_interval(&self) -> Duration {
        Duration::from_secs(self.aged_alarm_tick_secs)
    }

    /// Chain-verifier tick cadence as a [`Duration`].
    #[must_use]
    pub fn verify_interval(&self) -> Duration {
        Duration::from_secs(self.verify_tick_secs)
    }
}

/// ASC 606 revenue-recognition tunables (Slice 4, design §3.7 / §4.2).
///
/// `max_segments_per_schedule` is the **deployment** ceiling the pure
/// `ScheduleBuilder` enforces before materialization (decision 3 / E-8): a
/// straight-line schedule whose derived segment count exceeds it is blocked
/// (`DomainError::ScheduleTooLong`) rather than degraded — degrade (coarser /
/// chunked auto-segmentation) and a per-tenant ceiling are deferred (VHP-1853).
/// `recognition_run_tick_secs` is the cadence of the Phase 2 `RecognitionRunJob`
/// ticker (mirrors the `JobsConfig` `*_tick_secs` knobs).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecognitionConfig {
    /// Maximum `recognition_segment` rows one schedule may carry. The
    /// `ScheduleBuilder` blocks (no degrade in v1) above this. Default `120`
    /// (design E-8 guardrail); a deployment may raise/lower it, but it must
    /// stay `> 0` (an empty schedule is meaningless).
    pub max_segments_per_schedule: usize,
    /// Seconds between recognition-run ticks (the Phase 2 release job). Defaults
    /// to every 5 minutes — far tighter than the daily fiscal jobs so a due
    /// period's segments release promptly once the period opens.
    pub recognition_run_tick_secs: u64,
}

impl Default for RecognitionConfig {
    fn default() -> Self {
        Self {
            max_segments_per_schedule: 120,
            recognition_run_tick_secs: 300,
        }
    }
}

impl RecognitionConfig {
    /// Validate the recognition tunables.
    ///
    /// # Errors
    /// Returns `Err` if `max_segments_per_schedule` is `0` (a schedule with no
    /// segments cannot exist, so the guard would reject every schedule) or if
    /// `recognition_run_tick_secs` is `0` (`tokio::time::interval` panics on
    /// `Duration::ZERO`, aborting the serve task at runtime rather than failing
    /// loudly at `init()`).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_segments_per_schedule == 0 {
            return Err(ConfigError::MustBePositive {
                field: "recognition.max_segments_per_schedule",
            });
        }
        if self.recognition_run_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "recognition.recognition_run_tick_secs",
            });
        }
        Ok(())
    }

    /// Recognition-run tick cadence as a [`Duration`].
    #[must_use]
    pub fn recognition_run_interval(&self) -> Duration {
        Duration::from_secs(self.recognition_run_tick_secs)
    }
}

/// FX & multi-currency tunables (Slice 5, design §4.5 / §4.6 / §13 F2-F4).
///
/// `revaluation_enabled` is the FLEET DEFAULT for the Mode-B unrealized
/// revaluation run, applied to a tenant WITHOUT an explicit `fx_revaluation_mode`
/// row (off by default — fail-safe: a Mode-A tenant whose ERP revalues must NOT
/// double-count). Per-tenant Mode A/B resolution is the `fx_revaluation_mode`
/// config (VHP-1986); an explicit row OVERRIDES this flag. Staleness
/// follows F3: G10 currencies stale past `stale_g10_hours` (24h), others past a
/// tenant policy bounded by `stale_default_max_days` (≤ 7 days — a config above 7
/// is REJECTED at `validate`, no silent clamp). `provider_order` is the
/// deterministic fallback order `RateSource` resolves over the local store; empty
/// until an adapter is configured (cross-currency posts then block fail-safe).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FxConfig {
    /// Mode-B unrealized revaluation gate. Default `false` (fail-safe off).
    pub revaluation_enabled: bool,
    /// Hours past a snapshot's `as_of` after which a G10-currency rate is stale.
    /// Bounded `1..=168` (7 days) at `validate` — a larger window overflows
    /// `chrono::Duration::hours` in `rate_source::is_stale` and panics.
    pub stale_g10_hours: u64,
    /// Upper bound (days) on the per-tenant staleness threshold for non-G10
    /// currencies. A configured threshold above this is rejected at `validate`.
    pub stale_default_max_days: u64,
    /// Seconds between `RateSyncJob` ticks (pull `fetch_latest` into the store).
    pub rate_sync_tick_secs: u64,
    /// Seconds between `RevaluationRunJob` ticks (Mode-B period-end remeasure).
    pub revaluation_run_tick_secs: u64,
    /// Deterministic provider fallback order (by `provider_id`); empty until an
    /// adapter is configured.
    pub provider_order: Vec<String>,
    /// Vendor of the FX rate-provider plugin the `RateSyncJob` discovers each tick
    /// from the types-registry (`RateProviderPluginSpecV1` instances). Must match
    /// the external `bss-rate-provider` core gear's advertised `vendor`.
    pub provider_vendor: String,
}

impl Default for FxConfig {
    fn default() -> Self {
        Self {
            revaluation_enabled: false,
            stale_g10_hours: 24,
            stale_default_max_days: 7,
            rate_sync_tick_secs: 3_600,
            revaluation_run_tick_secs: 86_400,
            provider_order: Vec::new(),
            provider_vendor: "cf.bss".to_owned(),
        }
    }
}

/// Upper bound on `stale_g10_hours`: 7 days in hours, mirroring the 7-day cap on
/// `stale_default_max_days`. A larger value would flow to
/// `chrono::Duration::hours` in `rate_source::is_stale` and panic (the staleness
/// window overflows chrono's millisecond representation) — bound it at `validate`.
const MAX_STALE_G10_HOURS: u64 = 168;

/// Lower bound on `rate_sync_tick_secs`. The `FxNoFetchAttempted` alert reads a
/// `3 × rate_sync_tick_secs` window of the adapter's completion metrics, and one
/// source attempt can stay in flight for up to the adapter's 30 s per-source
/// timeout ceiling — with a sub-minute tick a single slow attempt could span the
/// whole alert window and read as "no attempt occurred". A 60 s floor keeps that
/// window at ≥ 180 s, and nothing needs sub-minute FX syncs against feeds that
/// publish daily.
const MIN_RATE_SYNC_TICK_SECS: u64 = 60;

impl FxConfig {
    /// Validate the FX tunables.
    ///
    /// # Errors
    /// Returns `Err` if `stale_default_max_days > 7` (F3: a threshold above the
    /// 7-day bound must be rejected, never silently clamped), if `stale_g10_hours`
    /// exceeds [`MAX_STALE_G10_HOURS`] (a larger window overflows
    /// `chrono::Duration::hours` and panics in `rate_source::is_stale`), if
    /// `rate_sync_tick_secs` is below [`MIN_RATE_SYNC_TICK_SECS`] (the
    /// `FxNoFetchAttempted` alert-window bound — see the constant's docs), or if
    /// `stale_g10_hours` / `revaluation_run_tick_secs` is `0` (a zero staleness
    /// window admits any rate; a zero tick panics `tokio::time::interval`).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.stale_default_max_days > 7 {
            return Err(ConfigError::AboveMax {
                field: "fx.stale_default_max_days",
                max: 7,
                reason: "F3 bound",
            });
        }
        if self.stale_g10_hours == 0 {
            return Err(ConfigError::MustBePositive {
                field: "fx.stale_g10_hours",
            });
        }
        if self.stale_g10_hours > MAX_STALE_G10_HOURS {
            return Err(ConfigError::AboveMax {
                field: "fx.stale_g10_hours",
                max: MAX_STALE_G10_HOURS,
                reason: "7-day bound",
            });
        }
        if self.rate_sync_tick_secs < MIN_RATE_SYNC_TICK_SECS {
            return Err(ConfigError::BelowMin {
                field: "fx.rate_sync_tick_secs",
                min: MIN_RATE_SYNC_TICK_SECS,
                reason: "FxNoFetchAttempted alert-window bound",
            });
        }
        if self.revaluation_run_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "fx.revaluation_run_tick_secs",
            });
        }
        // Matched literally against the vendor a `RateProviderPluginSpecV1`
        // instance advertises; a blank value silently matches nothing and
        // surfaces as "no source plugins registered" on the first sync tick.
        if self.provider_vendor.trim().is_empty() {
            return Err(ConfigError::Required {
                field: "fx.provider_vendor",
                reason: "matched literally against the rate-provider plugin's advertised vendor",
            });
        }
        Ok(())
    }

    /// Rate-sync tick cadence as a [`Duration`].
    #[must_use]
    pub fn rate_sync_interval(&self) -> Duration {
        Duration::from_secs(self.rate_sync_tick_secs)
    }

    /// Revaluation-run tick cadence as a [`Duration`].
    #[must_use]
    pub fn revaluation_run_interval(&self) -> Duration {
        Duration::from_secs(self.revaluation_run_tick_secs)
    }
}

/// Reconciliation & period-close tunables (Slice 7 Phase 3, design §4.3 / §4.5).
///
/// `recon_tick_secs` is the cadence of the `ReconciliationJob` ticker (near-real-time
/// invoice-completeness watermark + the periodic AR/PSP checks). `ar_tolerance_minor_per_k_lines`
/// is the AR↔derived rounding tolerance X4 (≤ N minor units per 1,000 posted lines; statutory
/// floors override — not modelled here). `manifest_enforcement` / `bill_run_enforcement` gate
/// whether the issued-invoice-manifest completeness check and the bill-run-finished assertion
/// BLOCK period close — default OFF (fail-safe) until the launch-blocking cross-team feeds are
/// live (design §0 decision 3 / §4.5 residual risk). `close_lock_timeout_ms` bounds the close
/// drain window under a sustained bill run (design §4.5).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconConfig {
    /// Seconds between `ReconciliationJob` ticks. Defaults to every 5 minutes (300) —
    /// tighter than the daily fiscal jobs so a missed posting / variance surfaces well
    /// before the close window.
    pub recon_tick_secs: u64,
    /// AR↔derived tie-out tolerance: max minor units of rounding-only variance per 1,000
    /// posted lines (X4). Default `1`.
    pub ar_tolerance_minor_per_k_lines: u32,
    /// Block period close on an unresolved invoice-completeness gap (`MISSED_POSTING`).
    /// Default `false` (the manifest feed is launch-blocking cross-team; inert until live).
    pub manifest_enforcement: bool,
    /// Block period close until the bill-run-finished control signal is asserted.
    /// Default `false` (the signal is launch-blocking cross-team; inert until live).
    pub bill_run_enforcement: bool,
    /// Upper bound (ms) on the close drain window / lock wait. Default `5000`.
    pub close_lock_timeout_ms: u64,
}

impl Default for ReconConfig {
    fn default() -> Self {
        Self {
            recon_tick_secs: 300,
            ar_tolerance_minor_per_k_lines: 1,
            manifest_enforcement: false,
            bill_run_enforcement: false,
            close_lock_timeout_ms: 5_000,
        }
    }
}

impl ReconConfig {
    /// Validate the recon tunables.
    ///
    /// # Errors
    /// Returns `Err` if `recon_tick_secs` is `0` (`tokio::time::interval` panics on
    /// `Duration::ZERO`, aborting the serve task) or if `close_lock_timeout_ms` is `0`
    /// (a zero drain window can never let a sustained bill run drain).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.recon_tick_secs == 0 {
            return Err(ConfigError::MustBePositive {
                field: "recon.recon_tick_secs",
            });
        }
        if self.close_lock_timeout_ms == 0 {
            return Err(ConfigError::MustBePositive {
                field: "recon.close_lock_timeout_ms",
            });
        }
        Ok(())
    }

    /// Reconciliation-tick cadence as a [`Duration`].
    #[must_use]
    pub fn recon_tick_interval(&self) -> Duration {
        Duration::from_secs(self.recon_tick_secs)
    }
}

/// Hard ceiling on `payments.max_invoices_per_allocation`. An allocation posts
/// one CR `AR` line per touched invoice plus the DR `UNALLOCATED` leg and (on a
/// cross-currency close) a net `FX_GAIN_LOSS` line; the engine caps an entry at
/// 1,000 lines (`LEDGER_ENTRY_TOO_LARGE`), so the touched-invoice count must
/// leave room for those two extra lines. A config above this is rejected (never
/// silently clamped) so a misconfiguration fails loud at `init()` rather than
/// deep in a post.
pub const MAX_INVOICES_PER_ALLOCATION_CEILING: usize = 998;

/// Payments & allocation tunables (Slice 3, design §Bounds).
///
/// `max_invoices_per_allocation` bounds the number of invoices ONE allocation
/// may **touch** (the AR legs it posts) — NOT the payer's open-invoice backlog,
/// which is read-only and uncapped. A split touching more than this rejects with
/// `ALLOCATION_TOO_LARGE`. Defaults to `500` (the PM-confirmed working default);
/// a deployment may lower it (tighter transactions) or raise it toward the
/// [`MAX_INVOICES_PER_ALLOCATION_CEILING`], but never past it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaymentsConfig {
    /// Max invoices one allocation may touch. `1..=MAX_INVOICES_PER_ALLOCATION_CEILING`.
    pub max_invoices_per_allocation: usize,
}

impl Default for PaymentsConfig {
    fn default() -> Self {
        Self {
            max_invoices_per_allocation:
                crate::infra::payment::allocate::MAX_INVOICES_PER_ALLOCATION,
        }
    }
}

impl PaymentsConfig {
    /// Validate the payments tunables.
    ///
    /// # Errors
    /// Returns `Err` if `max_invoices_per_allocation` is `0` (a zero cap rejects
    /// every allocation) or exceeds [`MAX_INVOICES_PER_ALLOCATION_CEILING`] (an
    /// allocation entry would then risk the engine's 1,000-line ceiling).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_invoices_per_allocation == 0 {
            return Err(ConfigError::MustBePositive {
                field: "payments.max_invoices_per_allocation",
            });
        }
        if self.max_invoices_per_allocation > MAX_INVOICES_PER_ALLOCATION_CEILING {
            return Err(ConfigError::AboveMax {
                field: "payments.max_invoices_per_allocation",
                max: MAX_INVOICES_PER_ALLOCATION_CEILING as u64,
                reason: "engine 1000-line entry ceiling",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
