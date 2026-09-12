//! The registry's two properties — every trigger has an owner, and each of the
//! ones this crate can fire fires **on its own**.
//!
//! The control case at the bottom is what makes the rest evidence: without a world
//! in which no trigger fires, every assertion here passes against a
//! `triggered_by_content` that answered `Some(..)` unconditionally.

use uuid::Uuid;

use super::{Trigger, triggered, triggered_by_content, triggered_by_row};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::{ChangeSet, PublishedPriceBaseline};
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::source_scan::{blank_comments_and_literals, matching_brace};
use time::OffsetDateTime;

fn at(year: i32) -> OffsetDateTime {
    utc_ymd_hms(year, 1, 1, 0, 0, 0)
}

fn key(currency: &str) -> ScopeKey {
    ScopeKey::new(
        PlanId::new(Uuid::from_u128(1)),
        CurrencyCode::new(currency).expect("a three-letter code"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(2)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the eight axes agree")
}

/// A published `flat` row on `currency` at `amount`.
fn row(currency: &str, amount: i64) -> PriceRecord {
    let mut shape = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    shape.amount_minor = Some(MinorAmount::new(amount).expect("a non-negative amount"));
    PriceRecord {
        price_id: Uuid::from_u128(0xd0_11),
        scope_key: key(currency),
        row: shape,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Published,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(2026),
        row_version: RowVersion::new(1),
    }
}

// ---------------------------------------------------------------------------
// The roster
// ---------------------------------------------------------------------------

/// Every registered trigger has an owning slice, **and the document opens**.
///
/// `owning_slice`'s own doc gives the field one purpose — *"a path rather than a
/// slice number, so a reader greps once"* — and a path that greps to nothing
/// serves it no better than a slice number would.
///
/// **This assertion used to be that the string was well formed**: `starts_with
///("design/")` and `contains(".md")`, under a doc claiming it asserted "the path is
/// a real one and not a placeholder". It asserted no such thing, and the gap was not
/// theoretical — it passed green over **two** wrong paths across **four** of the
/// eighteen triggers (`design/04-market-tax.md`, whose file is `04-currency-tax.md`,
/// and `design/09-overlays-groups.md`, whose file is `09-price-overlays.md`). A
/// shape check cannot tell a document from a plausible name for one, so this opens
/// the file instead.
///
/// The docs live one directory up from the crate, which is why the base is
/// `CARGO_MANIFEST_DIR/..` and not a relative path: a test's working directory is
/// the *workspace* root under one runner and the crate root under another, and a
/// relative path would make this assertion answer differently depending on how it
/// was invoked.
#[test]
fn every_trigger_names_a_design_document_that_opens() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs");
    for trigger in Trigger::ALL {
        let slice = trigger.owning_slice();
        assert!(
            slice.starts_with("design/") && slice.contains(".md"),
            "{trigger:?} must name the document that owns its subject, got {slice}"
        );
        let path = docs.join(slice);
        assert!(
            path.is_file(),
            "{trigger:?} names {slice}, which is not a file in the design set"
        );
    }
}

/// **The census behind the attestation.** A trigger that answers `true` on the
/// act half must be *named by a producing site outside this registry*.
///
/// `subject_exists_in_this_crate` is the one predicate in the file that nothing
/// checks: it is a hand-written `match`, and a `true` added to it compiles,
/// passes every other case here, and is read by later authors as a statement
/// that the work landed. That is not hypothetical — `bulkGroupMove` answered
/// `true` under a dated comment claiming both membership triggers were "paid
/// 2026-08-12", and **no file in `src/` constructed it**: the only `of_act` on
/// that plane always passes `ImmediateMembershipReresolution`, the move route
/// builds a single-payer set, and the sole other occurrence in the tree was a
/// test. The transcription case below could not see it — a transcription copies
/// whatever the `match` says.
///
/// # What is exempted, and why it is three names rather than a predicate
///
/// The **content half** is produced inside this module and by design has no call
/// site anywhere else: [`triggered_by_row`] mints `grandfatherHorizonTightening`
/// and `noComputableRowDelta`, and [`triggered_by_content`] mints
/// `planShapeRevisionContent`. Those two functions are the census's blind spot
/// and the three names below are transcribed from their bodies, in this file's
/// standing style — a roster that is copied reddens when the thing it copies
/// moves, which is the obligation it exists to create.
///
/// A **test file is not a producer.** `_tests.rs` is excluded for the reason the
/// finding turns on: a variant mentioned only by a case asserting about it is
/// exactly the state `bulkGroupMove` was in.
#[test]
fn every_act_half_trigger_answering_true_is_named_by_a_producing_site() {
    /// Minted inside this module by the content half, so no other file names
    /// them — transcribed from `triggered_by_row` and `triggered_by_content`.
    ///
    /// **`grandfatherHorizonTightening` left this list on 2026-08-16**, and the
    /// departure is the point rather than the bookkeeping. It was here because the
    /// horizon was a *diff* nobody could perform: the route S7 §5 declares was
    /// unmounted and no repository moved the column on a published row, so
    /// [`triggered_by_row`] was its only mint and the exemption was honest. With
    /// `infra::grandfather` mounted it is an **act** as well — one a surface
    /// declares, because the door refuses a loosening before the evaluator is
    /// reached and every call that gets there tightens — so the census can ask for
    /// its producer like any other. An exemption kept past the condition that
    /// earned it is the same defect this pair of censuses was built to find, one
    /// level up.
    const MINTED_BY_THE_CONTENT_HALF: &[&str] =
        &["noComputableRowDelta", "planShapeRevisionContent"];

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(
        sources.len() > 50,
        "the walk found {} files, which is not this crate",
        sources.len()
    );

    let mut unproduced: Vec<String> = Vec::new();
    for trigger in Trigger::ALL {
        if !trigger.subject_exists_in_this_crate()
            || MINTED_BY_THE_CONTENT_HALF.contains(&trigger.as_str())
        {
            continue;
        }
        if !is_produced(&sources, *trigger) {
            let sites = naming_sites(&sources, *trigger);
            let named = if sites.is_empty() {
                "no site at all".to_owned()
            } else {
                sites
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            unproduced.push(format!("{} [{named}]", trigger.as_str()));
        }
    }

    assert!(
        unproduced.is_empty(),
        "these triggers answer `subject_exists_in_this_crate() == true` and no file \
         in this crate outside the registry names them at a site anything reaches, so \
         the `true` attests to work no surface can perform: {unproduced:?}"
    );
}

/// **The other direction of the same census**, and it is not the mirror image of a
/// tidy rule — it is where the second defect of this shape actually was.
///
/// The census above walks the `true` side and asks for a producer. That catches an
/// attestation with no code and cannot, by construction, catch its **inverse**: a
/// trigger answering `false` that the crate does construct anyway. `planRetirement`
/// was exactly that. `infra::retirement::retire_in` builds
/// `ChangeSet::of_act(Trigger::PlanRetirement, ..)` on the mounted
/// `POST …/plans/{planId}/retire`, and the registry answered `false` for it — so the
/// module doc listed retirement among the subjects this crate has *"no table, no
/// entity and no surface"* for while a route was serving it, and
/// `only_the_triggers_with_a_subject_in_this_crate_answer_true`, being a
/// transcription of the `match`, agreed with the `match`.
///
/// Both directions are the same obligation stated once: **the answer and the code
/// must agree**. A `false` beside a producing site understates the crate exactly as
/// a `true` beside nothing overstates it, and the understatement is the more
/// dangerous of the two — it is read as "this slice is not here yet" by the next
/// author, who then builds the surface a second time.
///
/// The content half is exempt for the census above's reason, and inverted: those
/// three are minted *inside* the registry, so "no producing site outside it" is
/// their normal state whatever they answer.
///
/// **It shares [`is_produced`]'s reachability axis, and that is what makes a
/// `false` beside a live constructor legal** — but only while nothing calls it.
/// `revenueShareChange` answered `false` for a day with
/// `infra::bundle::rev_share_change_set` sitting in the tree building its change
/// set, because a `pub fn` no surface calls declares nothing (D-232, D-321). D-321
/// clause (3) wrote down what would happen next — *"the day it gains a caller the
/// `false`-side walk reddens and the registry has to flip it back"* — and on
/// 2026-08-16 that is what happened: `infra::bundle::declared_act` calls it, this
/// case reddened, and the arm moved. **The obligation ran in the direction the pair
/// exists for**, which is the first time either walk has been the thing that
/// noticed rather than a thing that was updated to agree.
#[test]
fn every_act_half_trigger_answering_false_is_named_by_no_producing_site() {
    /// [`every_act_half_trigger_answering_true_is_named_by_a_producing_site`]'s
    /// list, for its reason and with its 2026-08-16 departure.
    const MINTED_BY_THE_CONTENT_HALF: &[&str] =
        &["noComputableRowDelta", "planShapeRevisionContent"];

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(
        sources.len() > 50,
        "the walk found {} files, which is not this crate",
        sources.len()
    );

    let mut produced_anyway: Vec<String> = Vec::new();
    for trigger in Trigger::ALL {
        if trigger.subject_exists_in_this_crate()
            || MINTED_BY_THE_CONTENT_HALF.contains(&trigger.as_str())
        {
            continue;
        }
        if is_produced(&sources, *trigger) {
            let reached = naming_sites(&sources, *trigger)
                .iter()
                .filter(|site| site.reachable)
                .map(|site| site.at.clone())
                .collect::<Vec<_>>()
                .join("; ");
            produced_anyway.push(format!("{} [{reached}]", trigger.as_str()));
        }
    }

    assert!(
        produced_anyway.is_empty(),
        "these triggers answer `subject_exists_in_this_crate() == false` and a file \
         in this crate outside the registry constructs them anyway, at a site something \
         reaches, so the `false` denies work that is built and mounted: \
         {produced_anyway:?}"
    );
}

/// Does any source outside the registry name this trigger **in its code, at a
/// site something reaches**?
///
/// The one reading both census directions share, so they cannot drift into
/// disagreeing about what "produced" means.
///
/// # Prose does not produce anything, and this used to accept it
///
/// The match was over the file's raw text, so a **comment** naming
/// `Trigger::X` paid the census for it. That is not a hypothetical margin: the
/// crate's only construction of `Trigger::BundleComposition` is
/// `infra::bundle::composition_change_set`, and deleting it would have left the
/// trigger green off two line comments in `api::rest::bundles` and one line of
/// `infra::bundle`'s own module doc — the precise state `bulkGroupMove` was in,
/// which is the defect this census was built to find. A census that a comment can
/// satisfy attests to prose.
///
/// **Comments are stripped rather than a construction shape matched**, and the
/// choice is not a toss-up. The act half has two spellings — `ChangeSet::of_act`
/// for the policy, overlay, bundle, cutover, membership and retirement acts, and
/// `Op::registered_trigger` returning `Some(Trigger::WindowCancellation)` into
/// `ChangeSet::of_window_mutation` for the window plane — so a needle shaped like
/// `of_act(Trigger::` would answer `false` for two triggers that are produced,
/// and would have to be widened by hand every time a third spelling appears. The
/// census's question is *"does this crate's code name it"*, and the honest way to
/// narrow a textual match to that is to remove what is not code.
///
/// `crate::source_scan` is that removal, and it is **the same instrument**
/// `approval_repo_tests` uses to tell a writer of an `AuditSubjectKind` from a
/// doc comment quoting one — one reading, not two.
///
/// # A site nothing reaches produces nothing either, and that is the third axis
///
/// Both census directions asked whether the crate **names** the trigger. That is
/// not the same question as whether the crate can **answer** it, and the two came
/// apart on `revenueShareChange` (D-321): its only naming site was the body of
/// `infra::bundle::rev_share_change_set`, then a `pub fn` with no caller anywhere in
/// `src/` — so the trigger was "produced" by a function nothing calls, and the
/// `true` beside it read to every reader as *"this crate declares the act"*. The
/// register had said as much in prose since D-232 — *"a `pub fn` that builds a
/// change set is not a surface declaring an act"* — while neither instrument could
/// see it.
///
/// So a naming site counts only when the function containing it is **named
/// somewhere else in this crate's code**. Three properties of that, stated rather
/// than assumed:
///
/// * **It is one link, not a closure.** A closure needs a root set, and this
///   crate's roots are reached through a proc-macro expansion — `#[toolkit::gear]`
///   dispatches `serve`/`init` through a trait, and the entry name appears only
///   inside a string literal the blanking removes — so a transitive walk would
///   report the whole router dead and every trigger unproduced. One link catches
///   the defect that has now occurred three times (a constructor with **zero**
///   callers); a producer reached only from a caller that is itself unreached
///   would still pass, and that is the residual.
/// * **A definition sharing a name with another is credited with the other's
///   callers.** `fn new` is named everywhere. That errs toward calling a site
///   reachable, which weakens the census and never reddens it falsely.
/// * **A `_tests.rs` caller is not a caller**, for the same reason a test is not a
///   producer: [`rust_sources`] never loads one, so a construction exercised only
///   by a case asserting about it is exactly the state `bulkGroupMove` was in.
fn is_produced(sources: &[Source], trigger: Trigger) -> bool {
    naming_sites(sources, trigger)
        .iter()
        .any(|site| site.reachable)
}

/// One place this crate's code names a trigger, and whether anything reaches it.
struct Site {
    /// `<path>::<function>`, or `<path>::<module level>` — for the diagnostic, so
    /// a red census names the function rather than only the token.
    at: String,
    reachable: bool,
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reach = if self.reachable {
            "reached"
        } else {
            "which nothing in this crate calls"
        };
        write!(f, "{} ({reach})", self.at)
    }
}

/// Every site this crate's code names `trigger` at, reachable or not.
///
/// [`is_produced`]'s subject, and the two censuses' diagnostic: a `true` with no
/// reachable site has to say *which* function was supposed to be the declaration,
/// or the reader is back to grepping.
fn naming_sites(sources: &[Source], trigger: Trigger) -> Vec<Site> {
    let needle = format!("Trigger::{trigger:?}");
    let mut sites = Vec::new();
    for (owner, source) in sources.iter().enumerate() {
        for (at, _) in source.code.match_indices(&needle) {
            let (name, reachable) = match innermost_fn(&source.defs, at) {
                // A module-level item — a `const`, a `static` — is evaluated
                // wherever it is named, so there is no call to ask about.
                None => ("<module level>".to_owned(), true),
                Some(def) => (def.name.clone(), is_called(sources, owner, def)),
            };
            sites.push(Site {
                at: format!("{}::{name}", source.label),
                reachable,
            });
        }
    }
    sites
}

/// The innermost `fn` whose body contains `at`.
///
/// Innermost because a `fn` nested in another is reached through its parent's
/// body, so it is the nested one whose callers decide the site.
fn innermost_fn(defs: &[FnDef], at: usize) -> Option<&FnDef> {
    defs.iter()
        .filter(|def| (def.from..def.to).contains(&at))
        .max_by_key(|def| def.from)
}

/// Is this definition named anywhere in the crate's code **outside its own span**?
///
/// Its own span, so a recursive call does not keep a dead function alive, and so
/// the `fn name` of the definition itself is not read as a use of it.
fn is_called(sources: &[Source], owner: usize, def: &FnDef) -> bool {
    sources.iter().enumerate().any(|(index, source)| {
        source.code.match_indices(def.name.as_str()).any(|(at, _)| {
            let inside_itself = index == owner && (def.from..def.to).contains(&at);
            !inside_itself && whole_identifier(&source.code, at, def.name.len())
        })
    })
}

/// Is the `len` bytes at `at` a whole identifier, rather than part of a longer one?
///
/// Without it `retire` would be called by every mention of `retire_in`, and a
/// census that a substring can satisfy is the textual reading again one level down.
fn whole_identifier(code: &str, at: usize, len: usize) -> bool {
    let part = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = code.as_bytes();
    (at == 0 || !part(bytes[at - 1])) && bytes.get(at + len).is_none_or(|b| !part(*b))
}

/// One non-test source of this crate: its path, its code, and what it defines.
struct Source {
    /// Relative to the crate root, so a diagnostic names a file a reader can open.
    label: String,
    /// The file's text with every comment and every literal blanked to spaces.
    code: String,
    /// Every `fn` this file defines, in file order.
    defs: Vec<FnDef>,
}

/// One `fn` definition: its name and the span from the `fn` keyword to the byte
/// after its body's closing brace.
struct FnDef {
    name: String,
    from: usize,
    to: usize,
}

/// Every `fn` one blanked file defines, or the reason the scan cannot say.
///
/// Every `Err` is a **failure of the census's evidence** and never a tolerated
/// case, `approval_repo_tests::kinds_opened_in`'s rule: a signature the scan
/// cannot read is a body it cannot attribute a producing site to, and an
/// unattributed site reads as module-level — which reads as reachable. The
/// permissive direction is the silent one, so it is refused loudly instead.
///
/// Two shapes are skipped rather than refused, because neither is a definition:
/// `fn (` opens a function-**pointer** type, and a signature closed by `;`
/// declares a trait method or an `extern` symbol and has no body.
fn fn_defs(code: &str) -> Result<Vec<FnDef>, String> {
    let line_of = |at: usize| code[..at].matches('\n').count() + 1;
    let bytes = code.as_bytes();
    let part = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut defs = Vec::new();
    for (at, _) in code.match_indices("fn ") {
        if at > 0 && part(bytes[at - 1]) {
            continue;
        }
        let name: String = code[at + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let mut cursor = skip_space(bytes, at + 3 + name.len());
        if bytes.get(cursor) == Some(&b'<') {
            let close = close_angle(bytes, cursor).ok_or_else(|| {
                format!(
                    "{}: `fn {name}`'s generic parameters are never closed",
                    line_of(at)
                )
            })?;
            cursor = skip_space(bytes, close + 1);
        }
        if bytes.get(cursor) != Some(&b'(') {
            return Err(format!(
                "{}: `fn {name}` is followed by no parameter list this scan can read, so a \
                 producing site inside it would be attributed to no function at all",
                line_of(at)
            ));
        }
        let close = close_paren(bytes, cursor).ok_or_else(|| {
            format!(
                "{}: `fn {name}`'s parameter list is never closed",
                line_of(at)
            )
        })?;
        let (opener, byte) = signature_end(bytes, close + 1).ok_or_else(|| {
            format!(
                "{}: `fn {name}` is closed by neither a body nor a `;`",
                line_of(at)
            )
        })?;
        if byte == b';' {
            continue;
        }
        let end = matching_brace(code, opener)
            .ok_or_else(|| format!("{}: `fn {name}`'s body is never closed", line_of(at)))?;
        defs.push(FnDef {
            name,
            from: at,
            to: end + 1,
        });
    }
    Ok(defs)
}

/// The first index at or after `from` that is not ASCII whitespace.
fn skip_space(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

/// The index of the `>` closing the `<` at `open`.
///
/// `->` is stepped over: a generic parameter can be a `Fn(T) -> U` bound, and
/// counting that arrow's `>` would close the list early and put the parameter list
/// out of reach.
fn close_angle(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    let mut at = open;
    while at < bytes.len() {
        match bytes[at] {
            b'<' => depth += 1,
            b'-' if bytes.get(at + 1) == Some(&b'>') => at += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// The index of the `)` closing the `(` at `open`.
fn close_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where a signature ends: the `{` opening its body, or the `;` that says it has
/// none.
///
/// Bracket depth is counted so a return type carries neither past this: `-> [u8;
/// 32]` holds a `;` and `-> (A, B)` a paren pair, and reading either as the end of
/// the signature would attribute every site after it to the wrong function.
fn signature_end(bytes: &[u8], from: usize) -> Option<(usize, u8)> {
    let mut nesting = 0_usize;
    for (at, byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            b'(' | b'[' => nesting += 1,
            b')' | b']' => nesting = nesting.saturating_sub(1),
            b'{' | b';' if nesting == 0 => return Some((at, *byte)),
            _ => {}
        }
    }
    None
}

/// Every non-test Rust source of this crate, **blanked to its code**, with this
/// registry and its own cases removed.
///
/// The registry is excluded because it necessarily names every variant — the
/// enumeration, `ALL`, and three exhaustive `match`es — so leaving it in would
/// make the census answer `true` for everything.
///
/// The blanking is [`is_produced`]'s subject and its doc carries the argument:
/// what comes back has every comment and every string, char and raw-string
/// literal replaced by spaces, so a mention in prose is not a producing site.
fn rust_sources(dir: &std::path::Path) -> Vec<Source> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut bodies = Vec::new();
    let entries = std::fs::read_dir(dir).expect("the crate's source tree is readable");
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            bodies.extend(rust_sources(&path));
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_rust = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"));
        if !is_rust || name.ends_with("_tests.rs") || name == "triggers.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a readable source file");
        let code = blank_comments_and_literals(&text);
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let defs = fn_defs(&code).unwrap_or_else(|e| panic!("{label}:{e}"));
        bodies.push(Source { label, code, defs });
    }
    bodies
}

/// **The census reads code, and this is what says so over the real walk.**
///
/// Both assertions are needed and neither implies the other. The first is that
/// nothing the walk hands back is prose: this crate's doc comments are its
/// densest text, so a single surviving `///` means the sources are raw and a
/// comment naming a trigger pays the census for it. The second is the control
/// that the blanking has not simply eaten everything — a rule that returned empty
/// strings would satisfy the first assertion perfectly and make **every** trigger
/// read as unproduced, which the `true`-side census would then report as eleven
/// findings rather than as its own instrument being broken.
///
/// `Trigger::PlanRetirement` is the control's subject because that file carries
/// the token both ways: `infra::retirement` names it at line 498 inside a doc
/// comment and constructs it at 565, so a walk that kept the prose and a walk
/// that dropped the code are told apart here and nowhere else in the crate.
#[test]
fn the_census_walks_this_crates_code_and_not_its_prose() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);

    assert!(
        !sources.iter().any(|source| source.code.contains("///")),
        "a doc comment survived the walk, so the census is textual and prose naming a \
         trigger would pay it"
    );
    assert!(
        sources.iter().any(|source| source
            .code
            .contains("ChangeSet::of_act(Trigger::PlanRetirement")),
        "the walk dropped a real construction, so every trigger would read as unproduced"
    );
}

/// **The reachability axis is on**, said over the real walk and not over a snippet.
///
/// [`fn_defs`] returning nothing is the silent failure of the third axis: with no
/// definitions, every producing site falls through [`innermost_fn`]'s `None` arm,
/// is read as a module-level item, and counts as reached — so the census would go
/// back to checking naming and every case in this file would stay green while
/// saying it checked more. The two assertions are the two ways that happens: the
/// walk finding no definitions at all, and the walk finding them and attributing a
/// site to the wrong one.
///
/// `retire_in` is the subject for the same reason it is the blanking control's:
/// `infra::retirement` names `PlanRetirement` in a doc comment at one line and
/// constructs it inside that function at another, so a scan that lost the bodies
/// and a scan that kept them are told apart here.
#[test]
fn the_census_attributes_a_producing_site_to_the_function_that_contains_it() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);

    let defined: usize = sources.iter().map(|source| source.defs.len()).sum();
    assert!(
        defined > 500,
        "the walk found {defined} function definitions, which is not this crate — with none \
         of them every producing site reads as module-level, and module-level reads as reached"
    );

    let needle = "ChangeSet::of_act(Trigger::PlanRetirement";
    let owner = sources
        .iter()
        .find_map(|source| {
            let at = source.code.find(needle)?;
            innermost_fn(&source.defs, at).map(|def| def.name.clone())
        })
        .expect("the retirement act is constructed inside a function of this crate");

    assert_eq!(
        owner, "retire_in",
        "the site belongs to the function the whole census hangs on; attributing it elsewhere \
         asks the wrong function's callers"
    );
}

/// **A comment naming a trigger is not a producing site**, asked of the two
/// comments that actually pay for one.
///
/// Transcribed from `api::rest::bundles` at the time of writing — line 663's
/// *"which is what made `Trigger::BundleComposition`"* and line 796's heading —
/// because those two are the whole margin: the crate's only construction of that
/// trigger is `infra::bundle::composition_change_set`, and with the census
/// textual, deleting it left `bundleComposition` green off exactly this prose.
///
/// The positive control beside it is the construction itself. Without it the case
/// passes against a reading that found nothing anywhere.
///
/// It applies the blanking [`rust_sources`] applies, over text small enough to
/// read — the end-to-end evidence that the walk itself blanks is
/// [`the_census_walks_this_crates_code_and_not_its_prose`], and that is the case
/// that was red.
#[test]
fn prose_naming_a_trigger_does_not_produce_it_and_a_construction_does() {
    let prose = vec![source(
        "prose.rs",
        "    // answer `subject_exists_in_this_crate` while nothing could ever evaluate it.\n\
         \x20   // anywhere in the crate**, which is what made `Trigger::BundleComposition`\n\
         /// # This call is what makes `Trigger::BundleComposition` real\n",
    )];
    assert!(
        !is_produced(&prose, Trigger::BundleComposition),
        "a comment quoting a trigger is prose about a producer, not one"
    );

    // **The caller is part of the fixture now**, and it was not before: this
    // snippet was the constructor alone, which is the exact shape the reachability
    // axis refuses. A fixture carrying the fault a new rule judges is not evidence
    // for the rule — it is the rule's first violator.
    let code = vec![source(
        "bundle.rs",
        "pub fn composition_change_set() -> ChangeSet {\n    \
         ChangeSet::of_act(Trigger::BundleComposition, [])\n}\n\
         fn publish() -> ChangeSet { composition_change_set() }\n",
    )];
    assert!(
        is_produced(&code, Trigger::BundleComposition),
        "the construction the census exists to find must still be found"
    );
}

/// **A construction inside a function nothing calls produces nothing**, with the
/// same construction under a caller as the control.
///
/// The two snippets differ by one line, which is the whole claim: what the census
/// now reads is not the presence of the token but whether anything can reach the
/// body holding it. Without the second assertion the first passes against a rule
/// that answers `false` for everything — the failure mode
/// [`the_census_walks_this_crates_code_and_not_its_prose`] exists to catch one
/// axis down.
///
/// This is `revenueShareChange` at snippet size. Its constructor
/// (`infra::bundle::rev_share_change_set`) is a `pub fn` building
/// `ChangeSet::of_act(Trigger::RevenueShareChange, [])` that no file in `src/`
/// calls, and both censuses read it as a producing site for two waves.
#[test]
fn a_construction_in_a_function_nothing_calls_does_not_produce_it() {
    let orphan = vec![source(
        "bundle.rs",
        "pub fn rev_share_change_set() -> ChangeSet {\n    \
         ChangeSet::of_act(Trigger::RevenueShareChange, [])\n}\n",
    )];
    assert!(
        !is_produced(&orphan, Trigger::RevenueShareChange),
        "a `pub fn` with no caller declares nothing, whatever its body constructs"
    );

    let called = vec![source(
        "bundle.rs",
        "pub fn rev_share_change_set() -> ChangeSet {\n    \
         ChangeSet::of_act(Trigger::RevenueShareChange, [])\n}\n\
         fn publish() -> ChangeSet { rev_share_change_set() }\n",
    )];
    assert!(
        is_produced(&called, Trigger::RevenueShareChange),
        "one caller is the whole difference, and the census must see it"
    );
}

/// The reachability axis is not paid by a **substring** of a caller's name, nor by
/// the definition's own `fn` line.
///
/// Both are ways a one-link check quietly degrades into no check: `retire` would be
/// called by every mention of `retire_in`, and a definition is always named once —
/// by itself.
#[test]
fn a_definition_is_not_called_by_its_own_name_or_by_a_longer_one() {
    let itself = vec![source(
        "bundle.rs",
        "pub fn rev_share() -> ChangeSet {\n    \
         ChangeSet::of_act(Trigger::RevenueShareChange, [])\n}\n\
         fn other() -> u32 { let rev_share_change_set = 1; rev_share_change_set }\n",
    )];
    assert!(
        !is_produced(&itself, Trigger::RevenueShareChange),
        "`rev_share_change_set` contains `rev_share`, and a substring is not a call"
    );

    let recursive = vec![source(
        "bundle.rs",
        "pub fn rev_share() -> ChangeSet {\n    \
         let _ = rev_share();\n    \
         ChangeSet::of_act(Trigger::RevenueShareChange, [])\n}\n",
    )];
    assert!(
        !is_produced(&recursive, Trigger::RevenueShareChange),
        "a function calling itself is not a function something calls"
    );
}

/// A trait method **declaration** defines no body, and a site after one must not be
/// attributed to it.
///
/// The scanner's own hazard: a signature closed by `;` is followed by a `{` that
/// belongs to some later item, so reading the next brace as its body would swallow
/// every definition after it and hand their producing sites to a function with no
/// callers — a census that reddens on work that is fine, which is how an instrument
/// stops being believed.
#[test]
fn a_signature_closed_by_a_semicolon_opens_no_body() {
    let defs = fn_defs(&blank_comments_and_literals(
        "trait Declares { fn header(&self) -> [u8; 32]; }\n\
         fn produces() -> ChangeSet { ChangeSet::of_act(Trigger::PlanRetirement, []) }\n",
    ))
    .expect("the scan reads both signatures");

    let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
    assert_eq!(
        names,
        ["produces"],
        "only the definition has a body; the declaration's `[u8; 32]` also carries a `;` the \
         signature scan must not read as its end"
    );
}

/// One hand-written source, read exactly as [`rust_sources`] reads a file.
fn source(label: &str, text: &str) -> Source {
    let code = blank_comments_and_literals(text);
    let defs = fn_defs(&code).unwrap_or_else(|e| panic!("{label}:{e}"));
    Source {
        label: label.to_owned(),
        code,
        defs,
    }
}

/// Two triggers sharing a token would make a diagnostic unable to say which act
/// required the reviewer.
#[test]
fn every_trigger_carries_a_distinct_token() {
    let mut tokens: Vec<&str> = Trigger::ALL.iter().map(|t| t.as_str()).collect();
    let declared = tokens.len();
    tokens.sort_unstable();
    tokens.dedup();

    // The dedup is the whole assertion. Comparing `declared` against
    // `Trigger::ALL.len()` would restate how `tokens` was built — a 1:1 `map` over
    // that same roster — and hold for every possible `as_str`.
    assert_eq!(tokens.len(), declared, "every trigger needs its own token");
}

/// **The set whose subject this crate carries**, transcribed rather than counted.
///
/// The rest name a subject with no table, no entity and no surface here, and they
/// answer `false` so the registry does not read as incomplete. A variant added to the
/// `true` side without a writer would fail here — which is the "no token without a
/// writer" rule, asserted rather than described.
///
/// **`true` is about the subject and not about the act**, and one member of this list
/// is exactly that distinction: `grandfatherHorizonTightening` has the column, the
/// comparison and the record, and **no mounted surface can author the row pair it
/// compares** — the S7 route is unmounted and `insert_prepared` refuses a second draft
/// on an occupied key. The registry's module doc carries that at full strength; what
/// this list says is that the trigger has an owner here, which it does.
///
/// **`bundleComposition` and `revenueShareChange` joined on 2026-08-06**, when
/// Slice 8 landed four tables, the composition's revision lifecycle,
/// `infra::bundle`'s two `ChangeSet::of_act` declarations and three mounted routes.
/// This list is a **transcription**, so it reddened on the flip and was updated in
/// the same edit — which is the obligation it exists to create.
///
/// **`grandfatheringCutover` joined later than its own store by three commits**: it
/// waited for `infra::cutover::cutover_in` to *declare* the act through
/// `ChangeSet::of_act`, because that is what the predicate is about. A table is not
/// a declaration.
///
/// **`priceOverlayMutation` joined on the merge of Slice 9's overlay half**, and it
/// waited for the same thing. The strand landed three tables, three entities, a
/// revision lifecycle and four mounted operations, and the trigger stayed `false`
/// through all of it because the submit route wrote its `materiality` token as a
/// **literal** — so nothing in the crate constructed the change set the act half
/// reads back. `api::rest::overlays::overlay_submit_materiality` is the declaration,
/// and this list moved with it.
///
/// **`bundleComposition` and `revenueShareChange` were both on this list without
/// meeting that bar, and they left it by different doors.** Both were declared by
/// `pub fn`s in `infra::bundle` that nothing called — `publish_bundle` evaluated no
/// verdict at all — which D-232 recorded on 2026-08-06 and neither census could see,
/// because both asked whether the crate *names* the trigger and a constructor's own
/// body names it. `composition_change_set` got its caller on **2026-08-11**, when
/// `bundle_publish_materiality` replaced the route's hard-coded materiality literal
/// with an evaluated verdict; that is what earns its place here, on
/// `overlay_submit_materiality`'s precedent one plane over. `rev_share_change_set`
/// got none, so `revenueShareChange` **left this list on 2026-08-15** (D-321), and
/// what reddened is the reachability axis rather than this transcription — which
/// copies the `match` in both directions and could not have told anyone.
///
/// **`revenueShareChange` rejoined on 2026-08-16**, and what it waited for was not a
/// caller. D-321 could have written one in a line and refused to, because the call
/// would have changed no observable byte: the verdict carried no trigger, so both
/// bundle acts rendered identically and the only thing that would have noticed the
/// second constructor being called was the census that reads for it. What earns the
/// place here is `infra::bundle::declared_act` — a diff of the composition being
/// published against the last one that was ever live — together with
/// `MaterialityVerdict::trigger`, which makes its answer readable. A declaration
/// nothing can observe is not a declaration, and satisfying the instrument would
/// have been the instrument gamed by its own fix.
///
/// **`bulkGroupMove` left this list on 2026-08-14**, and it should never have
/// joined it: the flip credited `ApprovalService::submit_membership_move_on` with
/// a `ChangeSet::of_act` declaration that writer does not make, and the move route
/// builds a single-payer set and always declares
/// `immediateMembershipReresolution`. A transcription cannot see that — it copies
/// whatever the `match` says — which is why
/// `every_act_half_trigger_answering_true_is_named_by_a_producing_site` now
/// stands beside it, and why that census, not this list, is what reddened.
///
/// **`planRetirement` joined on 2026-08-15, and it had been due since
/// `infra::retirement` landed.** `retire_in` declares
/// `ChangeSet::of_act(Trigger::PlanRetirement, ..)` on the mounted
/// `POST …/plans/{planId}/retire`, over a subject that is a whole service here.
/// The registry said `false`. This transcription could not see that either — it
/// copies the `match` in **both** directions — and neither could the `true`-side
/// census, which never visits a `false` arm. The
/// `..._answering_false_is_named_by_no_producing_site` walk is what reddened.
#[test]
fn only_the_triggers_with_a_subject_in_this_crate_answer_true() {
    let reachable: Vec<&str> = Trigger::ALL
        .iter()
        .filter(|t| t.subject_exists_in_this_crate())
        .map(|t| t.as_str())
        .collect();

    assert_eq!(
        reachable,
        [
            "grandfatherHorizonTightening",
            "grandfatheringCutover",
            "immediateMembershipReresolution",
            "bulkGroupMove",
            "thresholdPolicyDiff",
            "priceOverlayMutation",
            "windowCancellation",
            "windowShortening",
            "bundleComposition",
            "revenueShareChange",
            "planRetirement",
            "noComputableRowDelta",
            "planShapeRevisionContent",
            "taxonomyValueMutation",
        ]
    );
}

/// The mirror of the case above, and the census this pair was missing.
///
/// Both walks in this file hunt for **producers** — one over the `true` side,
/// one over the `false` side — and neither of them counts the `false` side, so
/// nothing in the suite reddened when the module doc's own sentence about what
/// that side *is* went wrong. It went wrong on the same day it dates itself: it
/// said six and named *"a historical import"*, and D-330 struck historical
/// import from the design set on 2026-08-16, taking the `BackdateGrant`, the
/// `historical_import` resource label, the `POST /historical-imports` routes and
/// the `pricing_historical_price` store with it. `Trigger::ALL` never had such a
/// variant and could not: five arms answer `false`, not six (review Z3-1).
///
/// Transcribed rather than derived, for the reason the `true`-side case gives:
/// the value under test is what a reader auditing `inst-mat-registered` against
/// §3 step 4 checks the module doc against, and asserting the `match` against
/// itself would prove nothing about either.
#[test]
fn only_the_triggers_with_no_subject_in_this_crate_answer_false() {
    let absent: Vec<&str> = Trigger::ALL
        .iter()
        .filter(|t| !t.subject_exists_in_this_crate())
        .map(|t| t.as_str())
        .collect();

    assert_eq!(
        absent,
        [
            "retirementUnwindingACutover",
            "gaGateClearingRepublish",
            "prepaidGateClearingRepublish",
            "grantNonPriceField",
        ]
    );
    assert_eq!(
        absent.len() + 14,
        Trigger::ALL.len(),
        "the two sides partition the roster, so a variant added to neither census is a variant \
         this pair stopped covering"
    );
}

// ---------------------------------------------------------------------------
// The act half
// ---------------------------------------------------------------------------

/// D-62's cancel, declared by the surface performing it.
#[test]
fn a_window_cancellation_is_a_registered_act() {
    let change = ChangeSet::of_act(Trigger::WindowCancellation, [row("USD", 1000)]);

    assert_eq!(triggered(&change), Some(Trigger::WindowCancellation));
}

/// D-62's shortening `PATCH`, which is a different trigger from the cancel because
/// an auditor reading the stored verdict years later needs to know which act it
/// was — the two have different remedies and different blast radii.
#[test]
fn an_effective_to_shortening_is_a_registered_act() {
    let change = ChangeSet::of_act(Trigger::WindowShortening, [row("USD", 1000)]);

    assert_eq!(triggered(&change), Some(Trigger::WindowShortening));
}

/// D-10: any policy diff, direction-agnostic.
#[test]
fn a_threshold_policy_diff_is_a_registered_act() {
    let change = ChangeSet::of_act(Trigger::ThresholdPolicyDiff, []);

    assert_eq!(triggered(&change), Some(Trigger::ThresholdPolicyDiff));
}

/// **The control for the act half.** A plan-revision publish, a window schedule
/// and a lengthening `PATCH` are not on the list, and their materiality is the
/// threshold policy's question. Without this, the three above pass against a
/// `triggered` that answered `Some(..)` for anything.
#[test]
fn an_ordinary_publish_is_not_a_registered_act() {
    let change = ChangeSet::of_records([row("USD", 1000)]);

    assert_eq!(triggered(&change), None);
}

// ---------------------------------------------------------------------------
// The content half
// ---------------------------------------------------------------------------

/// `inst-mat-registered`'s first clause. The horizon moves earlier on a row whose
/// baseline carries a later one, which cuts a grandfathered subscriber's remaining
/// life at an unchanged price.
#[test]
fn a_horizon_tightening_is_a_registered_change() {
    // Hand-built rows, and they have to be: a published row and a draft successor on
    // one key is a state no mounted surface can reach (`insert_prepared` refuses the
    // second row, `update_draft` refuses the published one), so this asserts the
    // comparison rather than a path. See the module doc.
    let mut published = row("USD", 1000);
    published.grandfather_until = Some(at(2030));
    let mut tightened = published.clone();
    tightened.grandfather_until = Some(at(2028));

    assert_eq!(
        triggered_by_row(&tightened, &published),
        Some(Trigger::GrandfatherHorizonTightening)
    );
}

/// A horizon put on a row that had none is a tightening of infinity: an absent
/// horizon is indefinite.
#[test]
fn a_horizon_set_where_there_was_none_is_a_tightening() {
    let published = row("USD", 1000);
    let mut bounded = published.clone();
    bounded.grandfather_until = Some(at(2028));

    assert_eq!(
        triggered_by_row(&bounded, &published),
        Some(Trigger::GrandfatherHorizonTightening)
    );
}

/// Loosening is **not** a trigger here: `GRANDFATHER_LOOSEN_FORBIDDEN` refuses it
/// outright at publish, and a rule that also called it material would be a second
/// owner of one refusal.
#[test]
fn a_loosened_horizon_is_not_a_registered_change() {
    let mut published = row("USD", 1000);
    published.grandfather_until = Some(at(2028));
    let mut loosened = published.clone();
    loosened.grandfather_until = Some(at(2030));

    assert_eq!(triggered_by_row(&loosened, &published), None);
}

/// D-115's row half: `billingTiming` is Billing's sole deferral input and carries
/// no price delta at all.
#[test]
fn a_contract_field_change_is_a_registered_change() {
    let published = row("USD", 1000);
    let mut deferred = published.clone();
    deferred.billing_timing = Some("arrears".to_owned());

    assert_eq!(
        triggered_by_row(&deferred, &published),
        Some(Trigger::NoComputableRowDelta)
    );
}

/// **The one D-115 exists for.** A revision whose price rows are exactly what is
/// published has changed only the plan's shape — a trial stretched 7 → 90 days, a
/// GL code moved — so the per-row evaluation has nothing to trip on and the change
/// would have gone out approver-free under any configured threshold.
#[test]
fn a_pure_shape_revision_is_a_registered_change() {
    let published = row("USD", 1000);

    let trigger = triggered_by_content(
        &ChangeSet::of_records([published.clone()]),
        &PublishedPriceBaseline::of_records([published]),
    );

    assert_eq!(trigger, Some(Trigger::PlanShapeRevisionContent));
}

/// A change set with a row the baseline does not carry is not a shape revision: it
/// moved a row, and for **this** fixture `inst-mat-newrow` is what answers it.
///
/// **The qualifier is load-bearing, and its absence read as coverage** (review
/// Z20-1). The sentence used to say `inst-mat-newrow` answers it flatly, which is
/// true of the row built below — a new `EUR` key against a `USD`-only baseline, so
/// the row has no baseline *of its own* — and false of the class the test name
/// describes. [`super::moves_no_row`] answers `false` as soon as **any** row's
/// content differs from its published counterpart, so an *existing* row edited by
/// one minor unit silences this trigger just as well, and then no rule answers at
/// all: `inst-mat-newrow` does not apply, every per-row delta is `Below`, and the
/// verdict is `AutoPublishable` with whatever shape rode along.
///
/// That is D-319's recorded gap rather than a defect of this test, and it is pinned
/// on the side that costs by
/// [`crate::domain::materiality_tests::a_revision_that_moves_a_row_reaches_no_shape_trigger_however_its_shape_moved`],
/// which builds exactly that change set and asserts today's answer with its control.
/// A reader who arrives here needs to know this case is the **new-key** arm and not
/// the boundary of the class.
#[test]
fn a_change_set_carrying_a_new_row_is_not_a_shape_revision() {
    let published = row("USD", 1000);

    let trigger = triggered_by_content(
        &ChangeSet::of_records([published.clone(), row("EUR", 900)]),
        &PublishedPriceBaseline::of_records([published]),
    );

    assert_eq!(trigger, None);
}

/// **An empty change set on a published plan IS a shape revision**, and this test
/// asserted the opposite on a false premise.
///
/// It read: *"a plan that has never carried a price row has no shape content to have
/// changed, and `inst-mat-first` owns that world"*. `inst-mat-first` owns the world
/// with **no baseline**; the argument this function takes is a baseline, so every
/// call of it is about a plan that has published. A plan whose first published
/// revision carried no price row — the world
/// `rest_windows::a_plans_first_window_is_authorable_through_the_routes_after_an_empty_publish`
/// executes — reached the row walk with nothing to trip on and auto-published its
/// **second** revision under a configured policy. That revision's whole content is
/// the plan's shape, which is what D-115 exists for.
#[test]
fn an_empty_change_set_on_a_published_plan_is_a_shape_revision() {
    let trigger = triggered_by_content(
        &ChangeSet::of_records([]),
        &PublishedPriceBaseline::of_records([]),
    );

    assert_eq!(trigger, Some(Trigger::PlanShapeRevisionContent));
}

/// **The control for the content half**, and the world §3's residue names: a pure
/// amount change on unchanged geometry is no trigger at all, so what decides it is
/// the threshold. Every assertion above would pass without this one against a
/// `triggered_by_content` that never answered `None`.
#[test]
fn a_pure_amount_change_on_unchanged_geometry_is_no_trigger_at_all() {
    let published = row("USD", 1000);

    let moved = row("USD", 1050);

    assert_eq!(
        triggered_by_content(
            &ChangeSet::of_records([moved.clone()]),
            &PublishedPriceBaseline::of_records([published.clone()]),
        ),
        None,
        "a row that moved is not a shape revision"
    );
    assert_eq!(
        triggered_by_row(&moved, &published),
        None,
        "and nothing about the row itself is registered"
    );
}
