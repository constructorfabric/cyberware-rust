use super::*;
use crate::domain::instant::from_unix_millis;

/// One arriving schedule, spelled out so each case varies exactly one field.
fn arriving() -> NewMigration {
    NewMigration {
        migration_id: Uuid::from_u128(0x_a1_d0),
        tenant_id: Uuid::from_u128(0x_7e_41),
        source_plan_id: PlanId::new(Uuid::from_u128(0x_50_11)),
        source_revision: 3,
        target_plan_id: PlanId::new(Uuid::from_u128(0x_7a_22)),
        effective_at: from_unix_millis(1_800_000_000_000).expect("a valid instant"),
        announced_at: from_unix_millis(1_700_000_000_000).expect("a valid instant"),
        scope: serde_json::json!({"kind": "all"}),
        delta_report: serde_json::json!({"deltas": []}),
        created_by: Uuid::from_u128(0x_ac_70),
        created_at: from_unix_millis(1_700_000_000_000).expect("a valid instant"),
    }
}

/// The row that arriving schedule wrote, as the store then holds it.
fn held_from(new: &NewMigration) -> MigrationRecord {
    MigrationRecord {
        migration_id: new.migration_id,
        source_plan_id: new.source_plan_id,
        source_revision: new.source_revision,
        target_plan_id: new.target_plan_id,
        effective_at: new.effective_at,
        announced_at: new.announced_at,
        scope: new.scope.clone(),
        state: MigrationState::Scheduled,
        delta_report: new.delta_report.clone(),
        exclusion_snapshot: None,
        completion_record: None,
    }
}

/// The retry M2 exists to serve: the same body under the same id is a replay.
///
/// The control on the refusal beside it — without this, a comparison that refused
/// everything would look like a working guard.
#[test]
fn an_identical_resubmission_is_the_replay_it_has_always_been() {
    let new = arriving();
    assert!(StatedRequest::of(&new).matches(&held_from(&new)));
}

/// **The fields whose divergence changes what the migration does are compared, one
/// at a time.**
///
/// Each of these was discarded in silence until 2026-08-20: `insert_or_load` read
/// every `RecordNotInserted` as a replay and returned the stored row, so a
/// corrected resubmission under a spent `migration_id` was answered with the
/// schedule it was meant to replace (review 2026-08-19).
#[test]
fn a_resubmission_naming_a_different_request_is_not_a_replay() {
    let new = arriving();
    let held = held_from(&new);

    let other_source = NewMigration {
        source_plan_id: PlanId::new(Uuid::from_u128(0x_50_ff)),
        ..arriving()
    };
    assert!(
        !StatedRequest::of(&other_source).matches(&held),
        "a different retiring plan is a different migration"
    );

    let other_target = NewMigration {
        target_plan_id: PlanId::new(Uuid::from_u128(0x_7a_ff)),
        ..arriving()
    };
    assert!(
        !StatedRequest::of(&other_target).matches(&held),
        "a different target is the substitution this refusal is for"
    );

    let other_scope = NewMigration {
        scope: serde_json::json!({"kind": "subscriptions", "ids": ["a"]}),
        ..arriving()
    };
    assert!(
        !StatedRequest::of(&other_scope).matches(&held),
        "a different scope binds a different subscriber set"
    );
}

/// **A differing `effective_at` is still a replay, and that is a pin on an open
/// question rather than an endorsement.**
///
/// The finding this closes named `effective_at` among the discarded fields, and it
/// is the one of the four left out. Two cases pin the current reading deliberately —
/// `sqlite_migration_repo::a_retry_of_one_migration_id_returns_the_original_schedule_and_never_a_second`
/// at the repository seam and
/// `rest_migrations::a_replay_of_one_migration_id_answers_200_with_the_original_schedule`
/// through the route — and neither is a fixture carrying a fault by accident. See
/// [`StatedRequest`]'s doc: whose reading of `inst-ms-api` is right is a decision for
/// the layer that owns the replay arm.
///
/// This case exists so that flipping it is **visible**: adding `effective_at` to the
/// comparison reddens here first, next to the sentence saying who has to agree.
#[test]
fn a_differing_effective_date_is_still_a_replay_today() {
    let held = held_from(&arriving());
    let later = NewMigration {
        effective_at: from_unix_millis(1_900_000_000_000).expect("a valid instant"),
        ..arriving()
    };
    assert!(StatedRequest::of(&later).matches(&held));
}

/// The five fields that move between a call and its retry **by construction** are
/// outside the comparison, or M2's replay contract would be unreachable.
///
/// `announced_at` and `created_at` are minted from the act's stamp, `source_revision`
/// is read off the source plan at commit time, and `delta_report` is recomputed from
/// the target's current shape — so an honest retry differs in all four. `state` and
/// the two execution records are the row's own life and were never request input.
#[test]
fn a_retry_whose_minted_and_derived_fields_moved_is_still_a_replay() {
    let held = held_from(&arriving());
    let retry = NewMigration {
        source_revision: held.source_revision + 4,
        announced_at: held.announced_at + time::Duration::hours(2),
        delta_report: serde_json::json!({"deltas": ["a_field_the_target_gained"]}),
        created_by: Uuid::from_u128(0x_ac_ff),
        created_at: held.announced_at + time::Duration::hours(2),
        ..arriving()
    };
    assert!(
        StatedRequest::of(&retry).matches(&held),
        "the same stated request, re-derived: this is the retry the id exists for"
    );
}

/// `scope` is compared as a parsed value, so a round trip that reorders its keys
/// is not a different request.
///
/// Postgres stores it `jsonb` and normalises; `SQLite` stores it `text` and does
/// not. A text comparison would have made an identical retry read as a payload
/// mismatch on one engine and not the other, which is the shape a repository-level
/// refusal must never have.
#[test]
fn a_scope_whose_keys_round_tripped_in_another_order_is_the_same_request() {
    let new = NewMigration {
        scope: serde_json::json!({"kind": "subscriptions", "ids": ["a", "b"]}),
        ..arriving()
    };
    let held = MigrationRecord {
        scope: serde_json::json!({"ids": ["a", "b"], "kind": "subscriptions"}),
        ..held_from(&new)
    };
    assert!(StatedRequest::of(&new).matches(&held));
}
