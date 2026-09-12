//! `bss-pricing` configuration section.
//!
//! Every field has a launch default, so a `gears:` entry with no `config:`
//! block is a valid deployment. The numbers are the ratified NFR values
//! (`PRD.md` §14/§15, ratified 2026-07-28), not invented ones.
//!
//! **One field here does turn a fail-closed check off, and it is the only one**:
//! [`CatalogVersionRegistryConfig`]'s non-default mode replaces the fail-closed
//! `CatalogVersion` source with one that invents versions locally. Stating that no
//! field can would leave a reader looking for the exception nowhere. Every *other*
//! guard holds to it — the fixture gate in particular has no off switch and
//! [`FixturesConfig`] says why.
//!
//! **This section is per deployment, and four of its values are per tenant**
//! (D-152). The four §14 caps in [`LimitsConfig`] are the **default** a tenant
//! with no `pricing_policy_object` entry takes; the tenant's own value, when
//! there is one, is resolved by
//! [`PolicyObjectRepo`](crate::infra::storage::repo::PolicyObjectRepo) and is
//! what the authoring rules are built from. Reading a cap straight off this
//! struct on an authoring path is therefore the defect D-152 closed — every
//! tenant of a deployment sharing one limit — and the reason the caps are not
//! handed to the domain from here.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// The cross-gear `CatalogVersion` request budget a deployment that names none is
/// held to.
///
/// Short on purpose. The call is one round trip to a registry that answers from
/// its own store, and what the budget bounds is not the registry's latency but
/// the write transaction the caller is holding open while it waits — ten of the
/// twelve call sites are inside one.
pub const DEFAULT_REGISTRY_CALL_TIMEOUT_SECS: u64 = 5;

/// The largest cross-gear `CatalogVersion` budget a deployment may name.
///
/// A cap and not a suggestion: past this a mutating request holds its row locks
/// and a pool connection for longer than any operator would read as "the call is
/// still going", and the retry the resulting 503 asks for is cheaper than the
/// wait. Enforced by [`LimitsConfig::validate`], so an over-budget value stops
/// the boot instead of surfacing later as a stuck publish.
pub const MAX_REGISTRY_CALL_TIMEOUT_SECS: u64 = 60;

/// The largest idempotency-key retention window a deployment may name, in hours.
///
/// **Arithmetic rather than policy**, and stated so it is not read as a product
/// judgement: [`LimitsConfig::idempotency_key_ttl`] renders the knob as seconds,
/// and this is the last value whose seconds fit a `u64`. Enforced by
/// [`LimitsConfig::validate`], because the alternative is the multiplication
/// aborting a debug boot and wrapping a release one to a tiny window — which
/// disables idempotency replay silently, in the direction nobody checks.
pub const MAX_IDEMPOTENCY_KEY_TTL_HOURS: u64 = u64::MAX.div_euclid(3_600);

/// Root of the `bss-pricing` config section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BssPricingConfig {
    /// Emit the frozen event set to the broker. Default `false`: there is no
    /// event broker in this repository yet, and the publish path must not fail
    /// because a fan-out target is absent (the outbox row is still written —
    /// fan-out, not the transaction, is what this gates).
    ///
    /// # Nothing reads this flag, and the design set does not say who should
    ///
    /// Recorded rather than resolved, because resolving it
    /// would mean inventing an owner. What is measurable:
    ///
    /// * **Nothing acts on it.** `Module::init` reads it to `warn!` that it has no
    ///   effect, which is a report and not a producer; beyond that the field, one
    ///   assertion in [`config_tests`](crate::config::config_tests) and two doc
    ///   comments are its only mentions in the crate.
    /// * **Its gated component does not exist here.** The relay that would drain
    ///   `pricing_outbox` and fan out is not in this repository.
    ///   [`crate::infra::storage::repo::outbox_repo`] states the consequence for the
    ///   write side and holds to it: the flag "gates **fan-out, not the row**", so
    ///   `enqueue` is unconditional and never reads it — "a publish whose row was
    ///   skipped because fan-out was off would be a publish nothing could ever
    ///   replay." [`crate::infra::read_model`] gives the same argument for why no
    ///   inbound port is defined either.
    /// * **The design set is silent.** `events_enabled` appears in no PRD
    ///   requirement, no design slice and no decision; the word "relay" appears
    ///   nowhere in the gear's docs outside the review folder. D-66 settles that
    ///   `CatalogVersionPublished` is the *registry's* event and deliberately outside
    ///   this gear's frozen set, which moves an emitter out of scope and says nothing
    ///   about a drainer.
    ///
    /// So there are two defensible readings and this comment does not choose:
    ///
    /// 1. **Owed and dormant.** The outbox exists, `idx_pricing_outbox_undrained` is
    ///    a cursor written for a drainer, and the frozen event set is normative — so
    ///    something must eventually drain it, and this flag is that component's
    ///    switch, correctly defaulted off until it exists.
    /// 2. **Not this gear's at all.** The flag is an implementation-originated knob
    ///    for a component the design set never assigns here, in which case what is
    ///    owed is a decision deleting it rather than a producer honouring it.
    ///
    /// **No producer is implemented on either reading**, per the rule this gear
    /// applies to a rule with no operand: a knob whose owner the design set does not
    /// name is a design question, and building a reader would settle it by
    /// accident. What the flag is *not* is forgotten — the marker is here, at the
    /// declaration, where a reader looking for the switch arrives.
    pub events_enabled: bool,
    /// Background-job cadences.
    pub jobs: JobsConfig,
    /// Publish-time size and lifetime limits.
    pub limits: LimitsConfig,
    /// Where the joint conformance-fixture registry is read from.
    pub fixtures: FixturesConfig,
    /// Which `CatalogVersion` source the publish path talks to. Defaults to the
    /// fail-closed one; see [`CatalogVersionRegistryConfig`].
    pub catalog_version_registry: CatalogVersionRegistryConfig,
    /// Where the SKU pick-lists get their suggestions. Defaults to none; see
    /// [`ProductCatalogConfig`].
    pub product_catalog: ProductCatalogConfig,
}

impl BssPricingConfig {
    /// Validate every sub-section.
    ///
    /// # Errors
    /// [`ConfigError`] on the first invalid value; `init()` aborts loudly
    /// rather than booting a gear whose ticker would panic or whose caps would
    /// admit an unbounded plan.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.jobs.validate()?;
        self.limits.validate()?;
        self.fixtures.validate()
    }
}

/// Which `CatalogVersion` source the publish path talks to.
///
/// The registry gear (Product & SKU) is the sole legitimate incrementer and it is
/// not in this repository, so the default leaves the fail-closed source in place
/// and every publish answers `503`. That is correct for anything real, and it also
/// makes the whole publish-dependent half of the gear — windows, cutovers,
/// supersessions, repricing, migrations — unreachable on a deployment meant to
/// demonstrate it.
///
/// **The escape is deliberately awkward.** The mode is a named value rather than a
/// boolean, so a deployment cannot switch it on by writing `true` next to
/// something else, and so the file itself records what was chosen. See
/// [`crate::infra::local_dev_registry`] for what the non-default mode costs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogVersionRegistryConfig {
    /// Which source to use. Default [`CatalogVersionSource::Unconfigured`].
    pub mode: CatalogVersionSource,
}

/// Where SKU suggestions and tax-category definitions are read from.
///
/// Separate from [`CatalogVersionRegistryConfig`] although both are the Product
/// & SKU registry: one is a publish dependency that fails closed, the other a
/// read this gear can do without. A deployment may reasonably have neither, or
/// invent versions without fabricating a catalog, so they are not one switch.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProductCatalogConfig {
    /// Which source to use. Default [`ProductCatalogSource::Unconfigured`].
    pub mode: ProductCatalogSource,
}

/// The product-catalog sources a deployment may name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductCatalogSource {
    /// No catalog to ask. The default: the pick-lists offer what the tenant has
    /// already priced and say that nothing was asked, which is true.
    #[default]
    Unconfigured,
    /// Serve a **fabricated** static set from this process. Named at length so
    /// it cannot be selected without saying so: a deployment carrying this value
    /// is showing operators catalog content no registry issued. See
    /// [`crate::infra::local_dev_catalog`] for what that costs and how it is
    /// swept afterwards. The same opt-in also enables visibly prefixed demo
    /// tax-category definitions; the existing config token is retained.
    LocalDevStaticSkus,
}

/// The `CatalogVersion` sources a deployment may name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogVersionSource {
    /// Fail closed: no registry, so no publish becomes addressable. The default,
    /// and the only correct value while the registry gear does not exist.
    #[default]
    Unconfigured,
    /// Invent versions in this process. **Not a registry**, and named at length so
    /// that it cannot be selected without saying so: a deployment carrying this
    /// value is choosing versions no registry issued, which is the second
    /// incrementer `CatalogVersionRegistryV1` warns makes `CatalogVersion`
    /// unordered. For a stand with no registry to talk to, and nothing else.
    LocalDevInventedVersions,
}

/// Where the generated joint conformance-fixture registry lives
/// (`gears/bss/fixtures/corpus/registry.toml`), read once at init by
/// [`crate::infra::fixture_gate::FixtureGate`].
///
/// There is deliberately **no** field here that disables the gate. The only
/// thing a deployment may state is where the artifact is; whether a `modelKind`
/// is publishable is decided by the corpus, and a path that resolves to nothing
/// leaves the gate closed for every kind rather than open for any.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FixturesConfig {
    /// Path to the generated `registry.toml`. Relative paths resolve against
    /// the process working directory; the default is the in-repository location,
    /// which is what the workspace-root e2e deployment sees.
    pub registry_path: PathBuf,
}

impl Default for FixturesConfig {
    fn default() -> Self {
        Self {
            registry_path: PathBuf::from("gears/bss/fixtures/corpus/registry.toml"),
        }
    }
}

impl FixturesConfig {
    /// # Errors
    /// [`ConfigError::EmptyPath`] when the path is blank. An empty string would
    /// otherwise be accepted by `PathBuf` and fail at load, producing a
    /// permanently closed gate whose cause reads as a missing file rather than
    /// as the configuration mistake it is.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.registry_path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyPath {
                field: "fixtures.registry_path",
            });
        }
        Ok(())
    }
}

/// Cadences for the gear's background work.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "the shared `_secs` postfix is the UNIT, and these are deserialized config keys: an \
              operator reading `readmodel_degraded_after` cannot tell seconds from milliseconds, \
              and a misread of this particular knob is a Critical alarm at the wrong threshold"
)]
pub struct JobsConfig {
    /// How often the read-model warm re-drive sweeps for publishes whose
    /// projection has not completed. The publish→read-model propagation target
    /// is p95 ≤ 5s, and a degraded publish's re-drive continues past it, so the
    /// sweep runs at that order.
    pub readmodel_warm_tick_secs: u64,
    /// How long a `pricing_catalog_version_ref` may stay `pending` **and
    /// unobserved** before `pricing.catalogversion.commit_overdue` raises
    /// Critical. Default 300s = the ratified max batching delay (D-47: p95 ≤ 60s,
    /// max 5 min).
    pub catalog_version_overdue_secs: u64,
    /// How long a publish whose commit **has** been observed may stay unwarm
    /// before it is `PlanPublishDegraded`. Default 5s = §1.2's ratified
    /// publish→read-model propagation SLO.
    ///
    /// A second knob rather than a reuse of the one above, because D-166 clause
    /// (2) is the whole point: measured against the batching delay the degraded
    /// signal says nothing the overdue signal did not, and an operator cannot
    /// tell "the registry has not answered" from "the registry answered and the
    /// warm is failing". The two thresholds differ by two orders of magnitude
    /// precisely because they measure different waits.
    ///
    /// # **No cadence relation is enforced against this one, and that is the
    /// decision rather than the omission**
    ///
    /// Its sibling `catalog_version_overdue_secs` carries one
    /// ([`ConfigError::CadenceNotInsideThreshold`]) and this knob deliberately does
    /// not, because the two clocks start in different places. The overdue clock
    /// starts at `requested_at`, stamped by a **publish** between two passes, so the
    /// cadence is spent before the sweep ever looks and at `tick >= threshold` the
    /// first healthy question is already late. The degraded clock starts at
    /// `commit_observed_at`, which **this sweep stamps itself**
    /// (`readmodel_warm::observe_commit` runs inside the same pass that then
    /// projects), so a healthy publish is warmed in the pass that started its clock
    /// and never reaches this threshold at all. A pass at `tick == threshold` spends
    /// none of the budget waiting to look.
    ///
    /// That is why the defaults may be — and are — **equal** at 5s: the value is
    /// §1.2's ratified propagation SLO on the threshold side and the same order on
    /// the cadence side, and an arm demanding strict inequality here would refuse
    /// the shipped configuration to buy slack a self-started clock does not need.
    /// What the equality does mean is that a projection which fails in the pass that
    /// observed the commit is reported degraded by the next pass with no retry in
    /// between — which is the literal reading of the SLO the threshold **is**, and
    /// late-rather-than-false is the safe direction for a Critical.
    ///
    /// A review asked for the arm on this pair; it named the
    /// wrong one, and `config_tests::the_default_cadences_are_accepted_including_the_equal_warm_pair`
    /// is what keeps a later reader from adding it.
    pub readmodel_degraded_after_secs: u64,
    /// How many of **one tenant's** pending refs a warm pass reads. Default 500.
    ///
    /// Bounded so a transient registry outage cannot turn an unbounded backlog
    /// into a memory problem, and bounded **per tenant** because on a
    /// cross-tenant page one tenant's stuck backlog defers every other tenant's
    /// completions (D-163 clause 2). A tenant whose page fills gets no
    /// completion decision that pass: the pass cannot know a version's whole
    /// subject set, and the frontier lagging is the safe direction.
    pub pending_refs_per_tenant: u64,
    /// How many tenants holding pending refs one warm pass sweeps. Default 250.
    ///
    /// **This number and the tenant order are the owner's**, carried in §F.2 —
    /// D-163 says so rather than choosing them. The default is stated with its
    /// reasoning and nothing more: the discovery read costs one index seek per
    /// tenant that actually has an **outstanding publish**, and a healthy
    /// deployment holds a ref pending for at most a batching delay, so 250
    /// covers a deployment where a quarter of a thousand tenants are mid-publish
    /// at one 5s tick. Past the bound the **tail of the tenant order is not
    /// swept at all**, and the order is ascending tenant id rather than
    /// rotating, which needs a cursor no table in §3.7 carries.
    pub pending_tenants_per_pass: u64,
    /// How long a `pricing_price_window` boundary may stand uncrossed before
    /// `pricing.window.activation_overdue` raises Warn (`07-pricewindow-linkage.md`
    /// §7). Default 300s.
    ///
    /// **The design set names no value for this one**, which is why the default
    /// is stated with its reasoning rather than cited. §10 says the activation
    /// job's SLO *"rides the ratified NFR set (2026-07-28, PRD §14)"* and that
    /// set has no activation row at all — it ratifies publish propagation, read
    /// latency, price-row validation, event delivery, the currency floor and
    /// repricing throughput. So the value is the deployment's to state, and 300s
    /// is the order of the ratified max batching delay (D-47) on the argument
    /// that a window which has not flipped for five minutes is late by every
    /// other clock in this gear.
    ///
    /// A separate knob from the tick cadence below, and not a multiple of it:
    /// the cadence is how often the sweep looks, the threshold is how late a
    /// flip has to be before an operator is told. Deriving one from the other
    /// would mean an operator who slowed the sweep to save load silently also
    /// raised the alarm bar.
    ///
    /// **Separate is not unrelated, and the relation is validated**
    /// ([`ConfigError::CadenceNotInsideThreshold`]): the cadence must be strictly
    /// less than this threshold. A window whose boundary arrives just after a pass
    /// is found by the next pass a whole tick later, so at `tick >= threshold` the
    /// first and entirely healthy attempt at every flip is already reported late.
    /// Two independent values, one inequality between them — which is what this
    /// paragraph used to leave out, and what made the alarm's meaning a function of
    /// two knobs with only their zeroes refused.
    pub window_activation_overdue_secs: u64,
    /// How often the window activation/expiry sweep looks for a boundary that has
    /// arrived. Default 60s.
    ///
    /// **The cadence is the resolution of the whole plane**, which is why it is
    /// not the warm sweep's 5s and not a day: a window takes effect up to one tick
    /// after the instant it was scheduled for, and D-144 quantizes those instants
    /// to the millisecond. A minute is the value stated with its reasoning: the
    /// authoring floors that bound a changeover are D-47's **5 minutes**
    /// (`inst-gc-compose`, `inst-su-instant` — an instant must be at least the max
    /// batching delay in the future at approval commit), so a sweep an order of
    /// magnitude inside that floor cannot be what makes a changeover late, while a
    /// 5s sweep would spend twelve passes a minute reading an index for rows that
    /// are almost never there.
    pub window_activation_tick_secs: u64,
    /// How often the gated-market gauge is refreshed (D-250). Default 60s.
    ///
    /// Taken from `window_activation_tick_secs`'s reasoning rather than chosen:
    /// a periodic sweep an order of magnitude inside the bound its subject moves
    /// on cannot be what makes the value late, and a 5s tick would spend twelve
    /// passes a minute on work that almost never finds a change. The argument is
    /// stronger here than for windows — a changeover is bounded by D-47's
    /// five-minute authoring floor, while a market gated on tax-category
    /// readiness moves when an operator declares a category or the future Tax
    /// Engine answers, which the PRD's risk table sizes in **months**.
    ///
    /// **Do not tighten this.** The read is `price_repo::gated_markets`, a
    /// catalog-wide scan rather than the index probe the window sweep runs, and
    /// the value feeds §7's alarm on a condition an operator resolves in days.
    pub gated_markets_tick_secs: u64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            readmodel_warm_tick_secs: 5,
            catalog_version_overdue_secs: 300,
            readmodel_degraded_after_secs: 5,
            pending_refs_per_tenant: 500,
            pending_tenants_per_pass: 250,
            window_activation_overdue_secs: 300,
            window_activation_tick_secs: 60,
            gated_markets_tick_secs: 60,
        }
    }
}

impl JobsConfig {
    /// The read-model warm re-drive cadence.
    #[must_use]
    pub const fn readmodel_warm_interval(&self) -> Duration {
        Duration::from_secs(self.readmodel_warm_tick_secs)
    }

    /// The pending-and-unobserved `CatalogVersion` alarm threshold.
    #[must_use]
    pub const fn catalog_version_overdue_after(&self) -> Duration {
        Duration::from_secs(self.catalog_version_overdue_secs)
    }

    /// The observed-but-unwarm degraded threshold — §1.2's 5s propagation SLO.
    #[must_use]
    pub const fn readmodel_degraded_after(&self) -> Duration {
        Duration::from_secs(self.readmodel_degraded_after_secs)
    }

    /// The window activation/expiry sweep cadence.
    #[must_use]
    pub const fn window_activation_interval(&self) -> Duration {
        Duration::from_secs(self.window_activation_tick_secs)
    }

    /// The gated-market gauge's refresh interval (D-250).
    #[must_use]
    pub const fn gated_markets_interval(&self) -> Duration {
        Duration::from_secs(self.gated_markets_tick_secs)
    }

    /// The window activation/expiry overdue threshold — the Warn alarm's age
    /// bound.
    #[must_use]
    pub const fn window_activation_overdue_after(&self) -> Duration {
        Duration::from_secs(self.window_activation_overdue_secs)
    }

    /// # Errors
    /// [`ConfigError::ZeroInterval`] for a zero cadence: `tokio`'s interval
    /// panics on a zero period, and a zero alarm threshold would fire on every
    /// publish before the registry could possibly have answered.
    /// [`ConfigError::CadenceNotInsideThreshold`] when either sweep's cadence stands
    /// at or past the overdue threshold whose clock **a publish rather than the
    /// sweep** starts — the relations in this section that no single field's value
    /// can be judged by. Two pairs carry it: the window cadence against
    /// `window_activation_overdue_secs`, and the warm cadence against
    /// `catalog_version_overdue_secs`. `readmodel_degraded_after_secs` is
    /// deliberately not a third; see its own doc for the clock that exempts it.
    pub const fn validate(&self) -> Result<(), ConfigError> {
        if self.readmodel_warm_tick_secs == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.readmodel_warm_tick_secs",
            });
        }
        if self.catalog_version_overdue_secs == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.catalog_version_overdue_secs",
            });
        }
        if self.readmodel_degraded_after_secs == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.readmodel_degraded_after_secs",
            });
        }
        // A zero scan bound is not a "read nothing" instruction, it is a pass
        // that reads nothing and therefore never completes a version - a
        // frontier frozen at whatever it stood at, silently, forever.
        if self.pending_refs_per_tenant == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.pending_refs_per_tenant",
            });
        }
        if self.pending_tenants_per_pass == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.pending_tenants_per_pass",
            });
        }
        // `tokio::time::interval` panics on a zero period, so this one is the
        // difference between a boot that fails with a field name and a lifecycle
        // that dies on its first tick.
        if self.window_activation_tick_secs == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.window_activation_tick_secs",
            });
        }
        if self.gated_markets_tick_secs == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.gated_markets_tick_secs",
            });
        }
        // Zero here is not "alarm immediately", it is "alarm on every window the
        // sweep ever flips" - the Warn that means a stalled singleton, raised
        // once per window per healthy pass, which is how a real signal becomes
        // one an operator filters out.
        if self.window_activation_overdue_secs == 0 {
            return Err(ConfigError::ZeroInterval {
                field: "jobs.window_activation_overdue_secs",
            });
        }
        // The same pathology as the line above, at every value rather than only at
        // zero, and it is the reason this arm exists. The overdue condition is read
        // off the due set, so a boundary arriving just after a pass is found by the
        // next pass a whole tick later; with the cadence at or past the threshold
        // that first, healthy attempt is already late, and the Warn that means a
        // stalled singleton fires once per window on every ordinary flip. Refusing
        // only the zero left every such pair accepted - 600s/300s among them.
        //
        // Last of the arms on purpose: a zero is reported as a zero, which names
        // one field to fix instead of a relation between two.
        if self.window_activation_tick_secs >= self.window_activation_overdue_secs {
            return Err(ConfigError::CadenceNotInsideThreshold {
                cadence: "jobs.window_activation_tick_secs",
                threshold: "jobs.window_activation_overdue_secs",
            });
        }
        // The same relation on the warm plane, and the pair is chosen by **whose
        // clock the threshold is read off** rather than by which ticker the knobs
        // are named after.
        //
        // `pricing.catalogversion.commit_overdue` is measured from `requested_at`,
        // stamped by a publish in some other process between two passes
        // (`readmodel_warm::observe_commit_overdue`, `waited >= threshold`). So the
        // cadence eats into that budget exactly as it does on the window plane: a
        // ref requested one instant after a pass is a whole tick old the first time
        // anybody asks the registry about it, and at `tick >= threshold` the very
        // first, entirely healthy question is already reported as "the registry has
        // not answered" — this gear's Critical, whose remediation the log line calls
        // a registry re-request.
        if self.readmodel_warm_tick_secs >= self.catalog_version_overdue_secs {
            return Err(ConfigError::CadenceNotInsideThreshold {
                cadence: "jobs.readmodel_warm_tick_secs",
                threshold: "jobs.catalog_version_overdue_secs",
            });
        }
        Ok(())
    }
}

/// Publish-time size and lifetime limits (ratified 2026-07-28).
///
/// **The four caps are deployment *defaults*, not the values in force**
/// (D-152). Each is what a tenant with no `pricing_policy_object` entry is
/// governed by, which is what keeps the ratified launch numbers from moving; the
/// value an authoring run actually enforces comes from
/// [`PolicyObjectRepo::authoring_policy`](crate::infra::storage::repo::PolicyObjectRepo::authoring_policy).
/// The TTL below is not one of them — an idempotency window is a property of the
/// deployment's dedup store, not of a tenant's catalog policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Default soft cap on tier bands per price row.
    pub max_tier_bands_per_row: u32,
    /// Default soft cap on price rows per plan.
    pub max_price_rows_per_plan: u32,
    /// Default largest `n` a `customEveryN Days(n)` frequency may carry
    /// (`INVALID_CUSTOM_INTERVAL`, PRD §14 / AC #84).
    ///
    /// Unlike the two soft caps above this one is **hard**: P1 says an over-cap
    /// interval is rejected at authoring with no silent clamp, because a
    /// clamped interval is a billing period the operator did not author and
    /// would never see.
    pub max_custom_interval_days: u32,
    /// Default largest `n` a `customEveryN Months(n)` frequency may carry. The
    /// months cap is separate from the days cap because the two units bound
    /// different things; see [`LimitsConfig::max_custom_interval_days`] for the
    /// hard-cap note.
    pub max_custom_interval_months: u32,
    /// Client idempotency-key retention. A replay inside the window returns the
    /// stored response; outside it the key is forgotten and the call executes
    /// again, so this is a correctness-relevant duration, not a cache knob.
    pub idempotency_key_ttl_hours: u64,
    /// How long a cross-gear `CatalogVersion` request may take before the caller
    /// gives up on it (`infra::registry_deadline`).
    ///
    /// A **lifetime** limit like the TTL above and not a size cap, so it is a
    /// deployment value and not one of D-152's four per-tenant defaults: the
    /// budget bounds a transaction this deployment is holding open, which is not
    /// a property of whose catalog is being published.
    ///
    /// Bounded above as well as below — see [`MAX_REGISTRY_CALL_TIMEOUT_SECS`].
    pub registry_call_timeout_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tier_bands_per_row: 100,
            max_price_rows_per_plan: 500,
            max_custom_interval_days: 366,
            max_custom_interval_months: 24,
            idempotency_key_ttl_hours: 24,
            registry_call_timeout_secs: DEFAULT_REGISTRY_CALL_TIMEOUT_SECS,
        }
    }
}

impl LimitsConfig {
    /// The idempotency-key retention window.
    ///
    /// Total for every value [`LimitsConfig::validate`] admits: the multiplication
    /// is bounded by [`MAX_IDEMPOTENCY_KEY_TTL_HOURS`] at boot, so it neither
    /// aborts nor wraps here.
    #[must_use]
    pub const fn idempotency_key_ttl(&self) -> Duration {
        Duration::from_secs(self.idempotency_key_ttl_hours * 3_600)
    }

    /// The cross-gear `CatalogVersion` request budget.
    ///
    /// **Total, as the accessor above now is.** The knob is declared in
    /// seconds and `Duration::from_secs` is total over `u64`, so there is no
    /// multiply for a config value to overflow — the hours knob's `* 3_600`
    /// panics the boot for a value above `u64::MAX / 3_600`, and declaring a
    /// second knob in a coarser unit than it is used in would reproduce that
    /// shape for nothing. [`Self::validate`] still bounds the value, because the
    /// reason to refuse a five-hour budget is not overflow.
    #[must_use]
    pub const fn registry_call_timeout(&self) -> Duration {
        Duration::from_secs(self.registry_call_timeout_secs)
    }

    /// # Errors
    /// [`ConfigError::ZeroLimit`] for a zero cap: a zero band or row cap makes
    /// every plan unpublishable, a zero interval cap makes every custom
    /// frequency unpublishable (P1 requires `n > 0`, so no `n` could satisfy
    /// both bounds), and a zero TTL disables idempotency replay silently — all
    /// fail loudly at boot instead.
    pub const fn validate(&self) -> Result<(), ConfigError> {
        if self.max_tier_bands_per_row == 0 {
            return Err(ConfigError::ZeroLimit {
                field: "limits.max_tier_bands_per_row",
            });
        }
        if self.max_price_rows_per_plan == 0 {
            return Err(ConfigError::ZeroLimit {
                field: "limits.max_price_rows_per_plan",
            });
        }
        if self.max_custom_interval_days == 0 {
            return Err(ConfigError::ZeroLimit {
                field: "limits.max_custom_interval_days",
            });
        }
        if self.max_custom_interval_months == 0 {
            return Err(ConfigError::ZeroLimit {
                field: "limits.max_custom_interval_months",
            });
        }
        if self.idempotency_key_ttl_hours == 0 {
            return Err(ConfigError::ZeroLimit {
                field: "limits.idempotency_key_ttl_hours",
            });
        }
        if self.idempotency_key_ttl_hours > MAX_IDEMPOTENCY_KEY_TTL_HOURS {
            return Err(ConfigError::LimitAboveMaximum {
                field: "limits.idempotency_key_ttl_hours",
                maximum: MAX_IDEMPOTENCY_KEY_TTL_HOURS,
            });
        }
        if self.registry_call_timeout_secs == 0 {
            return Err(ConfigError::ZeroLimit {
                field: "limits.registry_call_timeout_secs",
            });
        }
        if self.registry_call_timeout_secs > MAX_REGISTRY_CALL_TIMEOUT_SECS {
            return Err(ConfigError::LimitAboveMaximum {
                field: "limits.registry_call_timeout_secs",
                maximum: MAX_REGISTRY_CALL_TIMEOUT_SECS,
            });
        }
        Ok(())
    }
}

/// A rejected configuration value. Carries the dotted field path so the boot
/// log names what to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A cadence or threshold was zero.
    #[error("`{field}` must be greater than zero")]
    ZeroInterval {
        /// Dotted path of the offending field.
        field: &'static str,
    },
    /// A size or lifetime cap was zero.
    #[error("`{field}` must be greater than zero")]
    ZeroLimit {
        /// Dotted path of the offending field.
        field: &'static str,
    },
    /// A lifetime cap stands above the largest value the gear will honour.
    ///
    /// Refused at boot rather than clamped, for the reason
    /// [`LimitsConfig::max_custom_interval_days`] gives for its own hard cap: a
    /// clamped value is one the operator did not author and would never see. It
    /// carries the maximum because the field name alone does not tell them what
    /// to write instead.
    #[error("`{field}` must not exceed {maximum}")]
    LimitAboveMaximum {
        /// Dotted path of the offending field.
        field: &'static str,
        /// The largest value the gear honours.
        maximum: u64,
    },
    /// A path was configured as the empty string.
    #[error("`{field}` must not be empty")]
    EmptyPath {
        /// Dotted path of the offending field.
        field: &'static str,
    },
    /// A sweep cadence stands at or past the alarm threshold it is measured
    /// against.
    ///
    /// The only **cross-field** refusal in this section, and it carries both
    /// names because neither alone tells an operator what to change: either knob
    /// may move, and which one should is theirs to decide.
    #[error(
        "`{cadence}` must be strictly less than `{threshold}`, or a boundary crossed on the very \
         first attempt is already reported late"
    )]
    CadenceNotInsideThreshold {
        /// Dotted path of the cadence.
        cadence: &'static str,
        /// Dotted path of the threshold it must stay inside.
        threshold: &'static str,
    },
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
