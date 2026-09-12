use super::*;
use time::OffsetDateTime;

fn ts(y: i32, m: u32, d: u32) -> OffsetDateTime {
    crate::domain::instant::utc_ymd_hms(y, m, d, 0, 0, 0)
}

fn cand(id: &str, open: i64, at: Option<OffsetDateTime>) -> Candidate {
    Candidate {
        invoice_id: id.to_owned(),
        open_minor: open,
        original_posted_at: at,
    }
}

#[test]
fn orders_by_date_then_invoice_id_with_none_last() {
    // C posted first, A second, B has no date ⇒ last. Lump fills all.
    let cands = [
        cand("B", 100, None),
        cand("A", 100, Some(ts(2026, 2, 1))),
        cand("C", 100, Some(ts(2026, 1, 1))),
    ];
    let out = oldest_first(&cands, 300, None);
    let order: Vec<&str> = out.iter().map(|a| a.invoice_id.as_str()).collect();
    assert_eq!(order, ["C", "A", "B"]);
}

#[test]
fn fills_min_of_remaining_and_open() {
    // First invoice open 300, lump 500 ⇒ first gets 300, second gets 200.
    let cands = [
        cand("A", 300, Some(ts(2026, 1, 1))),
        cand("B", 999, Some(ts(2026, 2, 1))),
    ];
    let out = oldest_first(&cands, 500, None);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[0],
        Allocated {
            invoice_id: "A".to_owned(),
            amount_minor: 300
        }
    );
    assert_eq!(
        out[1],
        Allocated {
            invoice_id: "B".to_owned(),
            amount_minor: 200
        }
    );
}

#[test]
fn lump_below_sum_open_stops_midway() {
    // Lump 150: A (open 100) filled, B gets the remaining 50, C nothing.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 100, Some(ts(2026, 2, 1))),
        cand("C", 100, Some(ts(2026, 3, 1))),
    ];
    let out = oldest_first(&cands, 150, None);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[0],
        Allocated {
            invoice_id: "A".to_owned(),
            amount_minor: 100
        }
    );
    assert_eq!(
        out[1],
        Allocated {
            invoice_id: "B".to_owned(),
            amount_minor: 50
        }
    );
}

#[test]
fn lump_above_sum_open_fills_all_no_negative() {
    // Lump 1000 > Σopen 300: every invoice filled to its open, leftover (700)
    // is implicit (not returned), no negatives.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 200, Some(ts(2026, 2, 1))),
    ];
    let out = oldest_first(&cands, 1000, None);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].amount_minor, 100);
    assert_eq!(out[1].amount_minor, 200);
    let total: i64 = out.iter().map(|a| a.amount_minor).sum();
    assert_eq!(total, 300);
}

#[test]
fn hint_moves_candidate_to_front() {
    // Without the hint order is A,B,C; hinting C pulls it first so it is paid
    // before the older A/B.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 100, Some(ts(2026, 2, 1))),
        cand("C", 100, Some(ts(2026, 3, 1))),
    ];
    let out = oldest_first(&cands, 100, Some("C"));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "C");
}

#[test]
fn hint_not_in_candidates_is_ignored() {
    // Hint names nobody ⇒ plain oldest-first order, A first.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 100, Some(ts(2026, 2, 1))),
    ];
    let out = oldest_first(&cands, 100, Some("ZZZ"));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "A");
}

#[test]
fn ties_break_on_smaller_invoice_id_first() {
    // Equal dates ⇒ invoice_id ascending: A before B.
    let cands = [
        cand("B", 100, Some(ts(2026, 1, 1))),
        cand("A", 100, Some(ts(2026, 1, 1))),
    ];
    let out = oldest_first(&cands, 200, None);
    let order: Vec<&str> = out.iter().map(|a| a.invoice_id.as_str()).collect();
    assert_eq!(order, ["A", "B"]);
}

#[test]
fn zero_and_negative_open_are_skipped() {
    // A open 0, B open -5 ⇒ both skipped; only C receives.
    let cands = [
        cand("A", 0, Some(ts(2026, 1, 1))),
        cand("B", -5, Some(ts(2026, 2, 1))),
        cand("C", 100, Some(ts(2026, 3, 1))),
    ];
    let out = oldest_first(&cands, 100, None);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "C");
    assert_eq!(out[0].amount_minor, 100);
}

#[test]
fn empty_candidates_yields_empty() {
    let out = oldest_first(&[], 1000, None);
    assert!(out.is_empty());
}

#[test]
fn highest_amount_orders_by_open_desc_then_invoice_id() {
    // Open balances B=300, A=100, C=200 ⇒ B, C, A regardless of date. Lump fills
    // all so order is observable.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 300, Some(ts(2026, 2, 1))),
        cand("C", 200, Some(ts(2026, 3, 1))),
    ];
    let out = highest_amount_first(&cands, 600, None);
    let order: Vec<&str> = out.iter().map(|a| a.invoice_id.as_str()).collect();
    assert_eq!(order, ["B", "C", "A"]);
}

#[test]
fn highest_amount_ties_break_on_smaller_invoice_id_first() {
    // Equal open balances ⇒ invoice_id ascending: A before B.
    let cands = [
        cand("B", 100, Some(ts(2026, 1, 1))),
        cand("A", 100, Some(ts(2026, 2, 1))),
    ];
    let out = highest_amount_first(&cands, 200, None);
    let order: Vec<&str> = out.iter().map(|a| a.invoice_id.as_str()).collect();
    assert_eq!(order, ["A", "B"]);
}

#[test]
fn highest_amount_hint_moves_candidate_to_front() {
    // Without the hint order is B (largest), then A; hinting A pulls it first.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 300, Some(ts(2026, 2, 1))),
    ];
    let out = highest_amount_first(&cands, 100, Some("A"));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "A");
}

#[test]
fn highest_amount_hint_not_in_candidates_is_ignored() {
    // Hint names nobody ⇒ plain highest-amount order, B (300) first.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 300, Some(ts(2026, 2, 1))),
    ];
    let out = highest_amount_first(&cands, 100, Some("ZZZ"));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "B");
}

#[test]
fn highest_amount_fill_and_leftover_match_oldest_total() {
    // Same lump, same candidates ⇒ identical Σ given and identical leftover
    // (not returned) regardless of strategy; only the per-invoice order differs.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 200, Some(ts(2026, 2, 1))),
    ];
    let oldest = oldest_first(&cands, 1000, None);
    let highest = highest_amount_first(&cands, 1000, None);
    let sum = |v: &[Allocated]| v.iter().map(|a| a.amount_minor).sum::<i64>();
    assert_eq!(sum(&oldest), sum(&highest));
    assert_eq!(sum(&highest), 300); // leftover 700 is implicit, not returned
}

#[test]
fn highest_amount_stops_at_remaining_zero() {
    // Lump 350: B (open 300) filled, A gets the remaining 50, C nothing.
    let cands = [
        cand("A", 200, Some(ts(2026, 1, 1))),
        cand("B", 300, Some(ts(2026, 2, 1))),
        cand("C", 100, Some(ts(2026, 3, 1))),
    ];
    let out = highest_amount_first(&cands, 350, None);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[0],
        Allocated {
            invoice_id: "B".to_owned(),
            amount_minor: 300
        }
    );
    assert_eq!(
        out[1],
        Allocated {
            invoice_id: "A".to_owned(),
            amount_minor: 50
        }
    );
}

#[test]
fn highest_amount_zero_and_negative_open_are_skipped() {
    // A open 0, B open -5 ⇒ both skipped; only C receives.
    let cands = [
        cand("A", 0, Some(ts(2026, 1, 1))),
        cand("B", -5, Some(ts(2026, 2, 1))),
        cand("C", 100, Some(ts(2026, 3, 1))),
    ];
    let out = highest_amount_first(&cands, 100, None);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "C");
    assert_eq!(out[0].amount_minor, 100);
}

#[test]
fn highest_amount_empty_candidates_yields_empty() {
    let out = highest_amount_first(&[], 1000, None);
    assert!(out.is_empty());
}

#[test]
fn select_split_dispatches_to_oldest_first() {
    // Oldest A (Jan) before B (Feb) even though B has the larger balance.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 300, Some(ts(2026, 2, 1))),
    ];
    let out = select_split(&cands, 100, None, PrecedenceStrategy::OldestFirst);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "A");
}

#[test]
fn select_split_dispatches_to_highest_amount_first() {
    // Largest B (300) before A (100) even though A is older.
    let cands = [
        cand("A", 100, Some(ts(2026, 1, 1))),
        cand("B", 300, Some(ts(2026, 2, 1))),
    ];
    let out = select_split(&cands, 100, None, PrecedenceStrategy::HighestAmountFirst);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].invoice_id, "B");
}

#[test]
fn policy_ref_parse_round_trip() {
    for strategy in [
        PrecedenceStrategy::OldestFirst,
        PrecedenceStrategy::HighestAmountFirst,
    ] {
        assert_eq!(
            PrecedenceStrategy::parse(strategy.policy_ref()),
            Some(strategy)
        );
    }
    assert_eq!(
        PrecedenceStrategy::OldestFirst.policy_ref(),
        "oldest-first.v1"
    );
    assert_eq!(
        PrecedenceStrategy::HighestAmountFirst.policy_ref(),
        "highest-amount-first.v1"
    );
}

#[test]
fn parse_unknown_policy_is_none() {
    assert_eq!(PrecedenceStrategy::parse("nope"), None);
    assert_eq!(PrecedenceStrategy::parse(""), None);
    assert_eq!(PrecedenceStrategy::parse("oldest-first.v2"), None);
}

#[test]
fn default_policy_equals_oldest_first_policy_ref() {
    assert_eq!(
        DEFAULT_PRECEDENCE_POLICY,
        PrecedenceStrategy::OldestFirst.policy_ref()
    );
}
