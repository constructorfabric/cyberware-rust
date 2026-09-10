//! Unit cases for [`super`] — the three weights, K3's boundary, D-108's
//! never-broken lock and the fail-closed posture an absent registry produces.

use uuid::Uuid;

use std::collections::BTreeMap;

use super::{
    Boundary, ContractLockSet, DeltaKind, DeltaReport, SubscriptionFacts, TargetShape, analyze,
    grant_totals,
};
use crate::domain::contracts::{EntitlementGrants, GrantSet};
use crate::domain::error::DomainError;
use crate::domain::plan_shape::{PhaseKind, PlanPhase};
use crate::domain::scope_key::PhaseId;

fn boundary(currency: &str, region: &str, frequency: &str) -> Boundary {
    Boundary {
        currency: currency.to_owned(),
        region: region.to_owned(),
        frequency: frequency.to_owned(),
    }
}

/// A subscriber the target covers perfectly: no delta of any weight.
fn clean(subscription_ref: Uuid) -> SubscriptionFacts {
    SubscriptionFacts {
        subscription_ref,
        boundary: boundary("USD", "EU", "monthly"),
        addon_sku_ids: Vec::new(),
        grants: vec![("seats".to_owned(), 10)],
        on_grandfathered_row: false,
    }
}

fn target() -> TargetShape {
    TargetShape {
        covered: vec![boundary("USD", "EU", "monthly")],
        offered_addon_sku_ids: Vec::new(),
        required_addon_sku_ids: Vec::new(),
        grants: vec![("seats".to_owned(), 10)],
    }
}

fn resolved_empty() -> ContractLockSet {
    ContractLockSet::resolved([])
}

// ---------------------------------------------------------------------------
// The clean case, which is what gives every case below its contrast.
// ---------------------------------------------------------------------------

#[test]
fn a_subscription_the_target_covers_produces_no_delta_at_all() {
    let report = analyze(&[clean(Uuid::now_v7())], &target(), &resolved_empty());

    assert!(report.deltas.is_empty());
    assert!(report.blocking().is_empty());
    assert!(report.excluded().is_empty());
    assert!(!report.locks_unresolved);
    assert!(report.ensure_schedulable().is_ok());
}

// ---------------------------------------------------------------------------
// `inst-md-locks` / `inst-cl-exclude` — excluded is not blocking.
// ---------------------------------------------------------------------------

#[test]
fn a_contract_locked_subscription_is_excluded_and_does_not_block_the_schedule() {
    // D-108: the lock is never broken, and the operator is owed nothing. A design
    // that made locks blocking would let one contracted account veto a whole
    // plan's consolidation.
    let locked = Uuid::now_v7();
    let free = Uuid::now_v7();
    let report = analyze(
        &[clean(locked), clean(free)],
        &target(),
        &ContractLockSet::resolved([locked]),
    );

    assert_eq!(report.excluded(), [locked].into_iter().collect());
    assert!(report.blocking().is_empty());
    assert!(
        report.ensure_schedulable().is_ok(),
        "a lock excludes, it never blocks"
    );
}

#[test]
fn a_locked_subscription_is_asked_nothing_else_so_it_cannot_block_on_a_plan_it_will_never_reach() {
    // The short-circuit is the rule, not an optimisation: reporting an
    // entitlement overflow on a target this subscriber will never be moved to
    // would make the schedule blocked on a subscription it already agreed to
    // leave alone.
    let locked = Uuid::now_v7();
    let mut facts = clean(locked);
    facts.boundary = boundary("JPY", "APAC", "annual"); // uncovered
    facts.grants = vec![("seats".to_owned(), 999)]; // overflowing

    let report = analyze(&[facts], &target(), &ContractLockSet::resolved([locked]));

    assert_eq!(report.deltas.len(), 1);
    assert!(report.deltas[0].kind.is_exclusion());
    assert!(report.blocking().is_empty());
}

#[test]
fn an_absent_lock_registry_excludes_every_subscription_and_says_so() {
    // `inst-cl-source`'s outage clause. There is no registry in the built system,
    // so this is the only value the analyzer is handed today.
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let report = analyze(
        &[clean(a), clean(b)],
        &target(),
        &ContractLockSet::fail_closed(),
    );

    assert_eq!(report.excluded(), [a, b].into_iter().collect());
    assert!(report.locks_unresolved);
    // The distinction the operator needs: excluded because nobody could be asked,
    // not because Contracts named them.
    for delta in &report.deltas {
        assert_eq!(
            delta.kind,
            DeltaKind::ContractLocked { unresolved: true },
            "the report must say the exclusion was fail-closed"
        );
    }
    assert!(report.ensure_schedulable().is_ok());
}

#[test]
fn a_resolved_registry_reports_a_positive_lock_distinctly_from_a_fail_closed_one() {
    let locked = Uuid::now_v7();
    let report = analyze(
        &[clean(locked)],
        &target(),
        &ContractLockSet::resolved([locked]),
    );

    assert_eq!(
        report.deltas[0].kind,
        DeltaKind::ContractLocked { unresolved: false }
    );
    assert!(!report.locks_unresolved);
}

// ---------------------------------------------------------------------------
// `inst-md-boundary` — K3.
// ---------------------------------------------------------------------------

#[test]
fn a_target_missing_the_frozen_currency_region_or_frequency_blocks() {
    // Cross-currency, cross-region and cross-frequency moves are cancel + new,
    // never an in-place PlanLink. All three axes are the same blocking fact,
    // which is why they travel as one value.
    for wrong in [
        boundary("EUR", "EU", "monthly"),
        boundary("USD", "APAC", "monthly"),
        boundary("USD", "EU", "annual"),
    ] {
        let mut facts = clean(Uuid::now_v7());
        facts.boundary = wrong.clone();
        let report = analyze(&[facts], &target(), &resolved_empty());

        assert_eq!(
            report.blocking().len(),
            1,
            "{wrong:?} should be a blocking boundary delta"
        );
        assert_eq!(
            report.blocking()[0].kind,
            DeltaKind::BoundaryUncovered { boundary: wrong }
        );
    }
}

#[test]
fn a_subscriber_leaving_a_grandfathered_row_is_informational_and_never_blocking() {
    // The operator sees the price impact before confirm; nothing is owed.
    let mut facts = clean(Uuid::now_v7());
    facts.on_grandfathered_row = true;
    let report = analyze(&[facts], &target(), &resolved_empty());

    assert_eq!(report.deltas.len(), 1);
    assert_eq!(report.deltas[0].kind, DeltaKind::LeavesGrandfatheredRow);
    assert!(!report.deltas[0].kind.is_blocking());
    assert!(!report.deltas[0].kind.is_exclusion());
    assert!(report.ensure_schedulable().is_ok());
}

// ---------------------------------------------------------------------------
// `inst-md-entitlements`.
// ---------------------------------------------------------------------------

#[test]
fn a_target_granting_less_of_a_key_is_a_blocking_overflow() {
    let mut shape = target();
    shape.grants = vec![("seats".to_owned(), 5)];
    let report = analyze(&[clean(Uuid::now_v7())], &shape, &resolved_empty());

    assert_eq!(
        report.blocking()[0].kind,
        DeltaKind::EntitlementOverflow {
            key: "seats".to_owned(),
            source_total: 10,
            target_total: 5,
        }
    );
}

#[test]
fn a_grant_key_the_target_never_mentions_is_zero_and_not_unconstrained() {
    // A key the target does not mention is capacity the subscriber stops having,
    // which is exactly the overflow this delta is about.
    let mut shape = target();
    shape.grants = Vec::new();
    let report = analyze(&[clean(Uuid::now_v7())], &shape, &resolved_empty());

    assert_eq!(
        report.blocking()[0].kind,
        DeltaKind::EntitlementOverflow {
            key: "seats".to_owned(),
            source_total: 10,
            target_total: 0,
        }
    );
}

#[test]
fn a_target_granting_more_is_not_an_overflow() {
    // Otherwise every generous consolidation would be blocking.
    let mut shape = target();
    shape.grants = vec![("seats".to_owned(), 50)];
    let report = analyze(&[clean(Uuid::now_v7())], &shape, &resolved_empty());

    assert!(report.deltas.is_empty());
}

#[test]
fn an_equal_grant_is_not_an_overflow() {
    // The boundary case of the comparison: strictly-less, not less-or-equal.
    let report = analyze(&[clean(Uuid::now_v7())], &target(), &resolved_empty());
    assert!(report.deltas.is_empty());
}

// ---------------------------------------------------------------------------
// `inst-md-addons`.
// ---------------------------------------------------------------------------

#[test]
fn an_addon_the_target_does_not_offer_blocks() {
    let addon = Uuid::now_v7();
    let mut facts = clean(Uuid::now_v7());
    facts.addon_sku_ids = vec![addon];
    let report = analyze(&[facts], &target(), &resolved_empty());

    assert_eq!(
        report.blocking()[0].kind,
        DeltaKind::AddOnInvalidOnTarget {
            addon_sku_id: addon
        }
    );
}

#[test]
fn an_addon_the_target_requires_and_the_subscriber_lacks_blocks() {
    let required = Uuid::now_v7();
    let mut shape = target();
    shape.required_addon_sku_ids = vec![required];
    shape.offered_addon_sku_ids = vec![required];
    let report = analyze(&[clean(Uuid::now_v7())], &shape, &resolved_empty());

    assert_eq!(
        report.blocking()[0].kind,
        DeltaKind::AddOnMissingRequired {
            addon_sku_id: required
        }
    );
}

#[test]
fn a_subscriber_already_carrying_the_required_addon_produces_no_delta() {
    let required = Uuid::now_v7();
    let mut shape = target();
    shape.required_addon_sku_ids = vec![required];
    shape.offered_addon_sku_ids = vec![required];
    let mut facts = clean(Uuid::now_v7());
    facts.addon_sku_ids = vec![required];

    assert!(
        analyze(&[facts], &shape, &resolved_empty())
            .deltas
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// The refusal.
// ---------------------------------------------------------------------------

#[test]
fn the_refusal_enumerates_every_blocking_delta_because_blocked_is_not_actionable() {
    let subscription = Uuid::now_v7();
    let addon = Uuid::now_v7();
    let mut facts = clean(subscription);
    facts.boundary = boundary("JPY", "APAC", "annual");
    facts.addon_sku_ids = vec![addon];

    let report = analyze(&[facts], &target(), &resolved_empty());
    let err = report.ensure_schedulable().unwrap_err();

    let DomainError::MigrationBlocked(detail) = err else {
        panic!("expected MigrationBlocked, got {err:?}");
    };
    assert!(detail.contains("boundary_uncovered"), "{detail}");
    assert!(detail.contains("addon_invalid_on_target"), "{detail}");
    assert!(detail.contains(&subscription.to_string()), "{detail}");
}

#[test]
fn an_empty_report_schedules() {
    assert!(DeltaReport::default().ensure_schedulable().is_ok());
}

#[test]
fn one_subscription_can_carry_several_blocking_deltas_at_once() {
    // The analyzer reports all of them rather than stopping at the first, because
    // an operator resolving them one round trip at a time is the cost.
    let addon = Uuid::now_v7();
    let required = Uuid::now_v7();
    let mut shape = target();
    shape.grants = vec![("seats".to_owned(), 1)];
    shape.required_addon_sku_ids = vec![required];
    shape.offered_addon_sku_ids = vec![required];

    let mut facts = clean(Uuid::now_v7());
    facts.boundary = boundary("JPY", "APAC", "annual");
    facts.addon_sku_ids = vec![addon];

    let report = analyze(&[facts], &shape, &resolved_empty());
    // boundary + invalid addon + missing required + entitlement overflow
    assert_eq!(report.blocking().len(), 4);
}

// ---------------------------------------------------------------------------
// D-253: the entitlement grant set, in the analyzer's vocabulary.
// ---------------------------------------------------------------------------

fn grant_phase(n: u128, ordinal: i32, converts_to: Option<u128>) -> PlanPhase {
    PlanPhase {
        phase_id: PhaseId::new(Uuid::from_u128(n)),
        kind: if converts_to.is_some() {
            PhaseKind::Trial
        } else {
            PhaseKind::Evergreen
        },
        display_name: None,
        ordinal,
        converts_to_phase_id: converts_to.map(|c| PhaseId::new(Uuid::from_u128(c))),
        phase_duration_days: converts_to.map(|_| 14),
        display_trial_days: None,
    }
}

fn grant_set(quotas: &[(&str, i64)], flags: &[(&str, bool)]) -> GrantSet {
    GrantSet {
        feature_flags: flags.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
        quotas: quotas.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
    }
}

/// **The two key spaces are namespaced**, and this is the case that says why:
/// `GrantSet` lets a quota and a flag share a name, and a flat merge would make
/// them one entry whose value is whichever was written second.
#[test]
fn a_quota_and_a_flag_of_the_same_name_stay_two_grants() {
    let grants = EntitlementGrants {
        plan_tier_ref: None,
        plan_level: grant_set(&[("beta", 500)], &[("beta", true)]),
        per_phase: BTreeMap::new(),
    };

    let totals = grant_totals(&grants, &[]);

    assert_eq!(
        totals,
        vec![("flag:beta".to_owned(), 1), ("quota:beta".to_owned(), 500)],
        "the collision is the whole reason for the prefixes"
    );
}

/// A granted flag is `1` and a withheld one is `0`, so `target < source` states
/// the true thing about a capability the target drops.
#[test]
fn a_withheld_flag_reads_as_zero_so_losing_one_is_an_overflow() {
    let grants = EntitlementGrants {
        plan_tier_ref: None,
        plan_level: grant_set(&[], &[("sso", false), ("api", true)]),
        per_phase: BTreeMap::new(),
    };

    let totals = grant_totals(&grants, &[]);

    assert_eq!(
        totals,
        vec![("flag:api".to_owned(), 1), ("flag:sso".to_owned(), 0)]
    );

    // And it composes with the rule that reads it: a source that granted `sso`
    // meets a target that withholds it, and that is a blocking delta.
    let target = TargetShape {
        covered: vec![boundary("USD", "EU", "monthly")],
        offered_addon_sku_ids: Vec::new(),
        required_addon_sku_ids: Vec::new(),
        grants: totals,
    };
    let subscriber = SubscriptionFacts {
        subscription_ref: Uuid::from_u128(1),
        boundary: boundary("USD", "EU", "monthly"),
        addon_sku_ids: Vec::new(),
        grants: vec![("flag:sso".to_owned(), 1)],
        on_grandfathered_row: false,
    };
    let report = analyze(&[subscriber], &target, &resolved_empty());
    assert!(
        report.deltas.iter().any(|d| matches!(
            &d.kind,
            DeltaKind::EntitlementOverflow { key, source_total: 1, target_total: 0 } if key == "flag:sso"
        )),
        "a dropped capability is an overflow: {report:?}"
    );
}

/// **The minimum across phases, not the plan-level set** — the case the whole
/// per-phase clause of D-253 turns on.
///
/// The target grants 100 seats at plan level and in its terminal phase, and 5
/// during its trial. A subscriber carrying 50 will meet the trial, so the
/// operator must be told before confirming; reading the plan-level set alone
/// would report no delta at all.
#[test]
fn a_phased_target_is_measured_at_its_stingiest_phase() {
    let trial = Uuid::from_u128(0x11);
    let mut per_phase = BTreeMap::new();
    per_phase.insert(trial, grant_set(&[("seats", 5)], &[]));

    let grants = EntitlementGrants {
        plan_tier_ref: None,
        plan_level: grant_set(&[("seats", 100)], &[]),
        per_phase,
    };
    let schedule = vec![grant_phase(0x11, 0, Some(0x22)), grant_phase(0x22, 1, None)];

    assert_eq!(
        grant_totals(&grants, &schedule),
        vec![("quota:seats".to_owned(), 5)],
        "the evergreen phase falls back to the plan level's 100; the trial's 5 is the minimum \
         and is what a migrated subscriber actually meets"
    );

    // The same grants read against no schedule are the plan-level set, which is
    // the unphased case and NOT the same answer -- stated as a contrast so the
    // minimum cannot be mistaken for an artefact of the fixture.
    assert_eq!(
        grant_totals(&grants, &[]),
        vec![("quota:seats".to_owned(), 100)]
    );
}

/// A key absent from one phase is zero there, so the minimum is zero: a phase
/// that does not mention a grant is a phase in which the subscriber lacks it.
#[test]
fn a_key_missing_from_one_phase_is_zero_across_the_target() {
    let mut per_phase = BTreeMap::new();
    per_phase.insert(Uuid::from_u128(0x11), grant_set(&[], &[]));

    let grants = EntitlementGrants {
        plan_tier_ref: None,
        plan_level: grant_set(&[("seats", 100)], &[("sso", true)]),
        per_phase,
    };
    let schedule = vec![grant_phase(0x11, 0, Some(0x22)), grant_phase(0x22, 1, None)];

    assert_eq!(
        grant_totals(&grants, &schedule),
        vec![("flag:sso".to_owned(), 0), ("quota:seats".to_owned(), 0)],
        "an authored-but-empty phase set grants nothing, and absent is zero"
    );
}

/// **Every `DeltaKind` is exactly one of blocking, exclusion, informational.**
///
/// `is_blocking` and `is_exclusion` are both `matches!` over hand-listed variants,
/// so a variant added to the enum answers `false` to both and joins the
/// informational class in silence - a new blocking condition that stops nothing.
/// The `match` below has no rest pattern, so the compiler is what notices.
#[test]
fn every_delta_kind_is_classified_exactly_once() {
    fn ordinal(kind: &DeltaKind) -> usize {
        match kind {
            DeltaKind::ContractLocked { .. } => 0,
            DeltaKind::EntitlementOverflow { .. } => 1,
            DeltaKind::AddOnInvalidOnTarget { .. } => 2,
            DeltaKind::AddOnMissingRequired { .. } => 3,
            DeltaKind::BoundaryUncovered { .. } => 4,
            DeltaKind::LeavesGrandfatheredRow => 5,
        }
    }

    let every = [
        DeltaKind::ContractLocked { unresolved: false },
        DeltaKind::EntitlementOverflow {
            key: "seats".to_owned(),
            source_total: 5,
            target_total: 1,
        },
        DeltaKind::AddOnInvalidOnTarget {
            addon_sku_id: Uuid::from_u128(0xa1),
        },
        DeltaKind::AddOnMissingRequired {
            addon_sku_id: Uuid::from_u128(0xa2),
        },
        DeltaKind::BoundaryUncovered {
            boundary: boundary("EUR", "eu", "monthly"),
        },
        DeltaKind::LeavesGrandfatheredRow,
    ];

    // **The roster is the enum's, and it takes two halves to make it so.** A match
    // whose arms merely list the variants constrains the arm list and nothing
    // else: a variant added to the enum forces a new arm, the author writes one,
    // and the array above stays short with the new variant unclassified and this
    // case green. So each arm answers a distinct ordinal and the array is required
    // to carry every one of them -- the compiler forces the arm, the arm forces an
    // ordinal, and the coverage check forces the array entry.
    let covered: std::collections::BTreeSet<usize> = every.iter().map(ordinal).collect();
    let highest = covered
        .iter()
        .copied()
        .max()
        .expect("the roster is non-empty");
    assert_eq!(
        covered,
        (0..=highest).collect::<std::collections::BTreeSet<usize>>(),
        "every ordinal `ordinal` can answer must appear in `every`; a variant given \
         an arm and left out of the array is unclassified here"
    );
    assert_eq!(
        covered.len(),
        every.len(),
        "and each exactly once, so a duplicate cannot stand in for a missing one"
    );

    for kind in &every {
        assert!(
            !(kind.is_blocking() && kind.is_exclusion()),
            "{} cannot both stop the run and be excluded from it",
            kind.as_str()
        );
    }

    let blocking: Vec<&str> = every
        .iter()
        .filter(|k| k.is_blocking())
        .map(DeltaKind::as_str)
        .collect();
    assert_eq!(
        blocking,
        [
            "entitlement_overflow",
            "addon_invalid_on_target",
            "addon_missing_required",
            "boundary_uncovered"
        ],
        "exactly the classes section 5 types MIGRATION_BLOCKED"
    );

    let excluded: Vec<&str> = every
        .iter()
        .filter(|k| k.is_exclusion())
        .map(DeltaKind::as_str)
        .collect();
    assert_eq!(
        excluded,
        ["contract_locked"],
        "a contract lock is excluded and never blocking - `inst-cl-exclude`'s asymmetry"
    );

    let informational: Vec<&str> = every
        .iter()
        .filter(|k| !k.is_blocking() && !k.is_exclusion())
        .map(DeltaKind::as_str)
        .collect();
    assert_eq!(
        informational,
        ["leaves_grandfathered_row"],
        "and the operator sees the price impact before confirm"
    );
}
