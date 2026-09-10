//! The supersession act's identity.
//!
//! Pure, and tested apart from the commit because it is what makes a **retry** the same
//! act and two changeovers **two** acts. Everything else in this module needs a database;
//! this needs three values.

use uuid::Uuid;

use super::supersession_unit_ref;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::money::CurrencyCode;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use time::OffsetDateTime;

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_4d7))
}

fn key(class: PriceEligibility) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_8e)),
        class,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("both classes pair with cohort none")
}

fn at(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 4, day, 0, 0, 0)
}

/// The rendering, pinned whole.
///
/// **A golden vector rather than the tautology this case used to be** (review
/// RUST-TEST-001, 2026-08-20). It called `supersession_unit_ref` twice with equal
/// arguments and asserted the two strings were equal, which holds for any pure
/// function of its inputs — the body could have been `String::new()` and the case
/// stayed green, so `inst-su-api`'s stability property was recorded as covered
/// while nothing observed it.
///
/// Every axis below is one a change could move: the segment order, the `/`
/// between the three components, the `|` between the key's ten, the instant's
/// millisecond format and its `Z`, and the `none` filling the two usage
/// positions. All of them are load-bearing — the string is a stored
/// `subject_ref` a retry looks itself up by, so a re-spelling silently opens a
/// second approval unit for an act that already has one.
const RENDERED: &str = "00000000-0000-0000-0000-0000000094d7/supersession/\
     00000000-0000-0000-0000-0000000094d7|USD|EU|base|\
     00000000-0000-0000-0000-00000000fa8e|all_subscriptions|recurring|none|none|none\
     /2099-04-01T00:00:00.000000Z";

#[test]
fn a_retry_of_one_request_names_one_act() {
    // The whole point: `inst-su-api` makes the act idempotent per
    // `(planId, scope key, changeover instant)`, and the subject is what a retry looks
    // itself up by. Two calls with those three equal must render one string — **and it
    // must be this string**, because equality with itself is a property of every
    // implementation and equality with a literal is a property of this one.
    let first = supersession_unit_ref(plan(), &key(PriceEligibility::AllSubscriptions), at(1));
    let again = supersession_unit_ref(plan(), &key(PriceEligibility::AllSubscriptions), at(1));

    assert_eq!(
        first, again,
        "a retry must name the act the first call named"
    );
    assert_eq!(
        first, RENDERED,
        "and the act's name is a stored `subject_ref`: a change to the segment order, either \
         separator or the instant format re-keys every unit already in the register"
    );
}

#[test]
fn two_changeovers_on_one_key_are_two_acts() {
    // The failure this guards is the one `infra::window`'s `unit_subject_ref` records for
    // a schedule: a subject that dropped a discriminator would let an approval taken for
    // one act authorize a different one. Here the changeover is the discriminator an
    // operator actually varies — the same key repriced twice, effective on two dates.
    let april = supersession_unit_ref(plan(), &key(PriceEligibility::AllSubscriptions), at(1));
    let may = supersession_unit_ref(plan(), &key(PriceEligibility::AllSubscriptions), at(2));

    assert_ne!(april, may);
}

#[test]
fn two_keys_at_one_changeover_are_two_acts() {
    // A mass reprice names **one** changeover for every row it touches
    // (`inst-su-instant`'s bulk clause), so the instant alone cannot discriminate: without
    // the key in the subject, one approval would authorize the whole run.
    let all = supersession_unit_ref(plan(), &key(PriceEligibility::AllSubscriptions), at(1));
    let new_only =
        supersession_unit_ref(plan(), &key(PriceEligibility::NewSubscriptionsOnly), at(1));

    assert_ne!(all, new_only);
}

#[test]
fn the_subject_names_the_plan_the_act_and_the_whole_key() {
    // Read rather than parsed anywhere, but its shape is the reviewer's and the
    // operator's, so the three components are asserted present. `supersession` is in it
    // because `pricing_approval.subject_kind` is `price_unit` for this unit — the token S5
    // §6 declares — and a subject that named only a price row could not be told from a
    // `PATCH`'s.
    let subject = supersession_unit_ref(plan(), &key(PriceEligibility::AllSubscriptions), at(1));

    assert!(
        subject.contains(&plan().get().to_string()),
        "got: {subject}"
    );
    assert!(subject.contains("supersession"), "got: {subject}");
    assert!(
        subject.contains(&key(PriceEligibility::AllSubscriptions).to_string()),
        "the whole ten-axis key, so two classes of one plan cannot collide: {subject}"
    );
    assert!(subject.contains("2099-04-01"), "got: {subject}");
}

#[test]
fn the_changeover_is_rendered_to_the_millisecond_the_store_quantizes_to() {
    // D-144 quantizes an authored instant to the millisecond and `plan_supersession`
    // refuses anything finer, so a subject rendered at second precision would map two
    // *distinct* legal changeovers onto one act — an approval of one authorizing the
    // other. RFC 3339 with milliseconds is what the rest of the crate renders instants
    // as, and it is what makes the subject as fine as the value it names.
    let base = at(1);
    let a_millisecond_later = base + time::Duration::milliseconds(1);

    assert_ne!(
        supersession_unit_ref(plan(), &key(PriceEligibility::AllSubscriptions), base),
        supersession_unit_ref(
            plan(),
            &key(PriceEligibility::AllSubscriptions),
            a_millisecond_later
        )
    );
}
