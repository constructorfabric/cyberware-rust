//! The chain-key constructors, and the one property none of them can be asserted
//! about after the fact: **two aggregates on one segment verify perfectly.**
//!
//! A collision here is the invisible defect [`super::policy_chain`]'s doc names — the
//! hash chain over an interleaved segment is internally consistent, every row's
//! `prev_hash` matches, and an auditor reading it is told a coherent story about a
//! plan that includes another aggregate's mutations. So the disjointness is asserted
//! where it is decided rather than where it would be discovered.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    AuditPosition, after_position, bulk_operation_chain, overlay_chain, payer_chain, plan_chain,
    policy_chain,
};
use crate::domain::instant::from_unix;
use crate::domain::scope_key::PlanId;

/// The version nibble — the 13th hex digit, `xxxxxxxx-xxxx-Vxxx-…`.
const VERSION_MASK: u128 = 0xF << 76;

fn version_nibble(id: Uuid) -> u8 {
    u8::try_from((id.as_u128() >> 76) & 0xF).expect("four bits")
}

/// The same 128 bits under a different version nibble.
///
/// What the payer cases need and the other three do not: `payer_tenant_id` is
/// AMS-supplied, so its nibble is whatever AMS put there and `Uuid::now_v7()`
/// samples exactly one of the sixteen values `payer_chain` has to accept.
fn under_version(id: Uuid, nibble: u8) -> Uuid {
    Uuid::from_u128((id.as_u128() & !VERSION_MASK) | (u128::from(nibble) << 76))
}

#[test]
fn overlay_chains_are_disjoint_from_plan_chains_by_construction() {
    // Not a sample of ids that happen not to collide — that would be the
    // improbability argument the module refuses. The three claims are structural and
    // each is checked as such.
    for _ in 0..64 {
        let overlay_id = Uuid::now_v7();
        let plan_id = PlanId::new(Uuid::now_v7());

        // (1) Every plan chain carries v7's nibble, every overlay chain carries 8, so
        //     no plan chain is any overlay chain — whatever the ids.
        assert_eq!(version_nibble(plan_chain(plan_id)), 7);
        assert_eq!(version_nibble(overlay_chain(overlay_id)), 8);

        // (2) …including the adversarial case the raw-id spelling would fail: the
        //     plan and the overlay carrying **the same** id. Under `plan_chain`'s
        //     spelling these two would be one segment.
        let shared = Uuid::now_v7();
        assert_ne!(overlay_chain(shared), plan_chain(PlanId::new(shared)));

        // (3) The policy segment is a constant with a zero timestamp, which `now_v7`
        //     cannot mint, so it is not any overlay's either.
        assert_ne!(overlay_chain(overlay_id), policy_chain());
    }
}

#[test]
fn distinct_overlays_get_distinct_chains() {
    // Injectivity, which the nibble rewrite could plausibly have cost: it is the
    // property that keeps two overlays' histories from being one segment, and it
    // holds because only the version nibble moves and every input carries the same
    // one.
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    assert_ne!(a, b, "two v7 ids, minted apart");
    assert_ne!(overlay_chain(a), overlay_chain(b));

    // And the rewrite is a rewrite rather than a mask: everything but the nibble
    // survives, so the chain still carries the overlay's timestamp and entropy.
    assert_eq!(
        overlay_chain(a).as_u128() & !VERSION_MASK,
        a.as_u128() & !VERSION_MASK
    );
}

#[test]
fn bulk_operation_chains_are_disjoint_from_plan_and_overlay_chains_by_construction() {
    // The same structural argument `overlay_chains_are_disjoint_from_plan_chains_by_construction`
    // makes, one nibble over: nibble `9` is disjoint from `7` (every plan chain) and
    // from `8` (every overlay chain and the policy chain) by construction, not by
    // the ids involved happening not to collide.
    for _ in 0..64 {
        let operation_id = Uuid::now_v7();
        let overlay_id = Uuid::now_v7();
        let plan_id = PlanId::new(Uuid::now_v7());

        assert_eq!(version_nibble(bulk_operation_chain(operation_id)), 9);
        assert_eq!(version_nibble(plan_chain(plan_id)), 7);
        assert_eq!(version_nibble(overlay_chain(overlay_id)), 8);

        // The adversarial case: one id minted once, read as all three kinds. Under a
        // raw-id spelling these would be one segment.
        let shared = Uuid::now_v7();
        assert_ne!(
            bulk_operation_chain(shared),
            plan_chain(PlanId::new(shared))
        );
        assert_ne!(bulk_operation_chain(shared), overlay_chain(shared));

        // The policy segment's fixed, zero-timestamp value is not any bulk
        // operation's either.
        assert_ne!(bulk_operation_chain(operation_id), policy_chain());
    }
}

#[test]
fn distinct_bulk_operations_get_distinct_chains() {
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    assert_ne!(a, b, "two v7 ids, minted apart");
    assert_ne!(bulk_operation_chain(a), bulk_operation_chain(b));

    assert_eq!(
        bulk_operation_chain(a).as_u128() & !VERSION_MASK,
        a.as_u128() & !VERSION_MASK
    );
}

#[test]
fn payer_chains_are_disjoint_from_every_other_chain_by_construction() {
    // `payer_tenant_id` is AMS-supplied and carries no version-nibble promise at
    // all, unlike the other three inputs above, which this gear mints as `v7`
    // itself. So the adversarial case here is not "a v7 id read as two kinds" but
    // "an id of *any* version read as a payer" — and the structural claim still
    // has to hold: nibble `A` and nothing else decides membership in this chain
    // space.
    //
    // **The payer ids are therefore minted across the whole nibble range**, this
    // one included. Every case here used `Uuid::now_v7()` for them, which sampled
    // one of the sixteen versions the input space actually holds and left the
    // "any version" the comment claims entirely untested (2026-08-20 review).
    for nibble in 0x0..=0xF {
        let payer_id = under_version(Uuid::now_v7(), nibble);
        let operation_id = Uuid::now_v7();
        let overlay_id = Uuid::now_v7();
        let plan_id = PlanId::new(Uuid::now_v7());

        assert_eq!(version_nibble(payer_chain(payer_id)), 0xA);
        assert_eq!(version_nibble(plan_chain(plan_id)), 7);
        assert_eq!(version_nibble(overlay_chain(overlay_id)), 8);
        assert_eq!(version_nibble(bulk_operation_chain(operation_id)), 9);

        // The adversarial case: one id minted once, read as every kind. Under a
        // raw-id spelling these would all be one segment.
        //
        // **This id stays `v7` while the payer ids above do not**, and the
        // asymmetry is the constructors': `plan_chain` is the plan id *verbatim*,
        // so what it can collide with is decided by the id space plan ids come
        // from — this gear's own `Uuid::now_v7()`. Sweeping this id over all
        // sixteen nibbles asserts that a plan id could carry `A`, which is the one
        // value that would make `plan_chain` and `payer_chain` agree, and no
        // writer in this gear can produce it. `overlay_chain` and
        // `bulk_operation_chain` rewrite their nibble and so are disjoint from
        // this space whatever they are handed.
        let shared = Uuid::now_v7();
        assert_ne!(payer_chain(shared), plan_chain(PlanId::new(shared)));
        assert_ne!(payer_chain(shared), overlay_chain(shared));
        assert_ne!(payer_chain(shared), bulk_operation_chain(shared));

        // The policy segment's fixed, zero-timestamp value is not any payer's
        // either.
        assert_ne!(payer_chain(payer_id), policy_chain());
    }
}

#[test]
fn distinct_payers_get_distinct_chains_whatever_version_their_ids_carry() {
    // Injectivity, asserted over the input space this constructor accepts rather
    // than over `now_v7` alone: two AMS ids may differ in their version nibbles as
    // well as in their bodies, and a rewrite that also disturbed the other 124
    // bits would collapse two payers' histories into one segment.
    for nibble in 0x0..=0xF {
        let a = under_version(Uuid::now_v7(), nibble);
        let b = under_version(Uuid::now_v7(), 0xF - nibble);
        assert_ne!(a, b, "two ids, minted apart");
        assert_ne!(payer_chain(a), payer_chain(b));
    }

    // And the rewrite is a rewrite rather than a mask: everything but the nibble
    // survives, so the chain still carries the payer's own 124 bits.
    let id = under_version(Uuid::now_v7(), 0x4);
    assert_eq!(
        payer_chain(id).as_u128() & !VERSION_MASK,
        id.as_u128() & !VERSION_MASK
    );
}

/// **Injectivity holds only up to the version nibble, and this is where that is
/// said.**
///
/// The other three constructors read ids *this gear mints as `v7`*, so their
/// inputs all carry one nibble and the rewrite cannot lose anything.
/// [`super::payer_chain`] reads an AMS-supplied id, whose nibble is a value
/// rather than a constant — so the four bits it overwrites are four bits of a
/// payer's identity, and two payer tenants differing **only** there land on one
/// segment. That is the module's own invisible defect: the interleaved chain
/// verifies perfectly.
///
/// Pinned rather than fixed, because a fix is a re-keying: `chain_id` is stored on
/// every `pricing_audit_log` row and feeds `genesis_prev_hash`, so changing how a
/// payer's segment is named orphans the segments already written. What that is
/// worth against a collision needing two ids to share 124 bits is an owner's call,
/// and this case is what makes it a decision instead of an assumption. Nothing in
/// this gear narrows the input: the id arrives from AMS.
#[test]
fn two_payer_ids_differing_only_in_the_version_nibble_share_one_chain() {
    let a = under_version(Uuid::now_v7(), 0x4);
    let b = under_version(a, 0x7);

    assert_ne!(a, b, "two distinct payer ids");
    assert_eq!(
        payer_chain(a),
        payer_chain(b),
        "the nibble is the one part of a payer id this key does not carry"
    );
}

// ---------------------------------------------------------------------------
// The keyset cursor's predicate (review Z2-10)
// ---------------------------------------------------------------------------

/// [`super::after_position`] rendered, on **both** query builders.
///
/// This walk had no unit-level owner at all: it was driven only through a store,
/// and its failure mode is *silently skipping or repeating an audit record* —
/// which is precisely the failure a keyset test is for, and precisely the one a
/// store-driven page over rows with distinct instants never reaches.
///
/// **The assertion is armed against the function's own stated claim.** Its doc
/// says the lexicographic `>` is spelled as an OR-of-ANDs rather than as a row
/// comparison *"because the two backends do not agree on tuple comparison
/// syntax, and a predicate that worked on one engine and not the other would
/// make the walk's guarantee a deployment property"*. So the operand under test
/// is the rendered SQL on each engine, and the property is that the two render
/// the same shape. A `(a, b, c) > (?, ?, ?)` rewrite — the obvious tidy-up —
/// reddens here, on the engine it would break.
fn rendered(position: AuditPosition) -> (String, String) {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query, SqliteQueryBuilder};

    let condition = after_position(position).expect("a non-negative seq converts");
    let mut select = Query::select();
    select
        .expr(sea_orm::sea_query::Expr::cust("1"))
        .cond_where(condition);
    (
        select.to_string(SqliteQueryBuilder),
        select.to_string(PostgresQueryBuilder),
    )
}

#[test]
fn the_cursor_predicate_is_the_same_three_tier_shape_on_both_engines() {
    let (sqlite, postgres) = rendered(AuditPosition {
        recorded_at: from_unix(1_700_000_000, 0).expect("a valid instant"),
        chain_id: Uuid::from_u128(0x5eed),
        seq: 7,
    });

    for (engine, sql) in [("sqlite", &sqlite), ("postgres", &postgres)] {
        // Tier 1: a strictly later instant, on its own.
        assert!(
            sql.contains("\"recorded_at\" >"),
            "{engine}: the instant must advance strictly, or a page repeats its own last row: \
             {sql}"
        );
        // Tier 2 and 3: the tie-breaks, each gated on equality of the tier above.
        // `=` on the instant is what stops the walk skipping every row that
        // shares the cursor's instant — the whole reason this is not a bare `>`.
        assert!(
            sql.contains("\"recorded_at\" =") && sql.contains("\"chain_id\" >"),
            "{engine}: rows sharing the cursor's instant must be broken by chain, or the walk \
             skips every one of them: {sql}"
        );
        assert!(
            sql.contains("\"chain_id\" =") && sql.contains("\"seq\" >"),
            "{engine}: rows sharing instant and chain must be broken by seq, which is the only \
             total order left: {sql}"
        );
        // And it must not have become a tuple comparison, which is the rewrite
        // the function's doc exists to refuse.
        assert!(
            !sql.contains("(\"recorded_at\", "),
            "{engine}: a row-value comparison is what the OR-of-ANDs is written out to avoid: \
             {sql}"
        );
    }

    // The claim itself: one predicate, not two. Both builders inline the bound
    // values, so this is the whole rendered shape rather than a fragment of it.
    assert_eq!(
        sqlite, postgres,
        "the two engines must carry one predicate; a shape that differs on one of them makes \
         the walk's guarantee a deployment property, which is the sentence this function's own \
         doc gives as its reason for existing"
    );
}

#[test]
fn a_seq_beyond_the_columns_width_is_a_corrupt_row_and_not_a_silent_wrap() {
    // The positive control first: an ordinary seq converts, so the refusal below
    // is about the value and not about the function refusing everything.
    let ok = after_position(AuditPosition {
        recorded_at: from_unix(1_700_000_000, 0).expect("a valid instant"),
        chain_id: Uuid::from_u128(1),
        seq: u64::try_from(i64::MAX).expect("i64::MAX is a u64"),
    });
    assert!(ok.is_ok(), "the widest value the column holds must convert");

    let err = after_position(AuditPosition {
        recorded_at: from_unix(1_700_000_000, 0).expect("a valid instant"),
        chain_id: Uuid::from_u128(1),
        seq: u64::MAX,
    });
    assert!(
        matches!(err, Err(crate::infra::storage::RepoError::CorruptRow(_))),
        "a seq the signed column cannot hold is a corrupt cursor, never a wrapped one: {err:?}"
    );
}
