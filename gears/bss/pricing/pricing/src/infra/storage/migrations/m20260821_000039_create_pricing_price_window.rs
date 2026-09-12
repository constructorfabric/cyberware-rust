//! Create `bss.pricing_price_window` — the time axis of a published price
//! (`design/07-pricewindow-linkage.md` §6, `cpt-cf-bss-pricing-state-price-window`),
//! the slice-owned store D-03 puts in this gear rather than in the effective-dating
//! UC it absorbed.
//!
//! One row is one half-open interval `[effective_from, effective_to)` on one
//! price row, and therefore on that row's canonical scope key. **Half-open is
//! the load-bearing part**: `effective_to = next.effective_from` is adjacency
//! and not a gap (§9 names that false positive as one the coverage checker must
//! not produce), and `chk_pricing_price_window_interval` is written to admit it.
//! `effective_to IS NULL` is open-ended — a window that never expires
//! (`inst-ws-expire`) — and not a missing value.
//!
//! The §4 state machine lives in the constraints and not only in the domain, for
//! `pricing_plan`'s reason: a rule that lives only in application code is one
//! ad-hoc `UPDATE` away from being bypassed, and what it would be bypassing here
//! is which price a subscriber was charged over an interval that has already
//! been billed. `scheduled -> active` (`inst-ws-activate`),
//! `scheduled -> cancelled` (`inst-ws-cancel`), `active -> expired`
//! (`inst-ws-expire`), and nothing else; `expired` and `cancelled` are immutable
//! history (`inst-ws-immutable`).
//!
//! # The audit timestamps: what is actually true, and why the naive `CHECK` is
//! not
//!
//! The three flip timestamps are constrained as **biconditionals against the
//! reachable state set**, which is narrower than "one column per state":
//!
//! * `(state IN ('active','expired')) = (activated_at IS NOT NULL)`. An
//!   `expired` window **was** `active`, so it carries `activated_at` too — the
//!   naive `(state = 'active') = (activated_at IS NOT NULL)` would make every
//!   expiry unstorable, which is the form an implementer writes first. The
//!   left-hand side names two states for that reason.
//! * `cancelled` is deliberately **not** in that set, and that is a fact about
//!   §4 rather than an omission: the only edge into `cancelled` leaves
//!   `scheduled` (`inst-ws-cancel` — "an active or historical window is never
//!   cancelled"), so a cancelled window never activated and an `activated_at` on
//!   one is a record of something that did not happen.
//! * `(state = 'expired') = (expired_at IS NOT NULL)` and
//!   `(state = 'cancelled') = (cancelled_at IS NOT NULL)` are the plain
//!   biconditionals, in `chk_pricing_approval_decided_at`'s idiom.
//!
//! Both directions are asserted on purpose. The one-way form
//! `state IN ('scheduled','cancelled') OR activated_at IS NOT NULL` refuses an
//! `active` row with no `activated_at` and **accepts** a `scheduled` row that
//! claims to have been activated, which is a lie the store would then hold about
//! when a price took effect. §6 states no co-nullability at all, so this is a
//! reading of §4's edges rather than a transcription, and it is reported as one.
//!
//! # §4's transition **conditions**, and the three of them a row-local CHECK
//! carries
//!
//! §4's edges are conditional and the biconditionals above say nothing about the
//! conditions: `inst-ws-activate` fires **WHEN `now ≥ effectiveFrom`**,
//! `inst-ws-expire` **WHEN `now ≥ effectiveTo`**, and — verbatim — *"an
//! open-ended window never expires"*. Three of those conditions are decidable
//! from one row's own immutable columns, so three CHECKs carry them:
//!
//! * `chk_pricing_price_window_activation_order` —
//!   `activated_at IS NULL OR activated_at >= effective_from`. The stamped
//!   instant of an activation cannot precede the start it was the arrival of. A
//!   row that claims otherwise is the "activation that never happened" the
//!   INSERT-guard note below names, and it was reachable through
//!   [`window_repo::transition`](crate::infra::storage::repo::window_repo::transition)
//!   until 2026-08-04.
//! * `chk_pricing_price_window_expiry_order` —
//!   `expired_at IS NULL OR expired_at >= effective_to`. The same rule on the
//!   other edge. It is deliberately **silent** when `effective_to` is NULL (the
//!   comparison is then NULL and a CHECK admits NULL), because that case has its
//!   own constraint below and one statement refused by two of them is a removal
//!   proof that proves neither.
//! * `chk_pricing_price_window_open_ended` —
//!   `NOT (state = 'expired' AND effective_to IS NULL)`. `inst-ws-expire`'s own
//!   sentence, made physical: there is no instant an open-ended window's expiry
//!   could be the arrival of.
//!
//! **This is not the whole of §4's conditions and the residue is named.** The
//! two clock-dependent halves — that `now` really has reached the boundary at the
//! moment of the flip — are not here, because a CHECK cannot read the clock
//! without becoming the one guard in the chain that answers differently depending
//! on when it is asked. What these three give is the durable half: whatever the
//! flip's own instant was, the row cannot record it as having happened before the
//! boundary it was triggered by. The transient half is
//! [`window_repo::transition`](crate::infra::storage::repo::window_repo::transition)'s,
//! and it carries the condition **into the `UPDATE`'s `WHERE`** rather than only
//! into a pre-check, so a sweep and a concurrent writer cannot step between the
//! read and the write.
//!
//! # `effective_from` is frozen **always**, which is stricter than
//! `inst-ws-immutable`
//!
//! The instruction freezes `effective_from` *once it has passed*. The whitelist
//! below freezes it from the moment the row exists, and that is a **deliberate
//! narrowing of the instruction's literal scope**, recorded here and reported
//! rather than left to read as the rule:
//!
//! * there is no sanctioned writer of a scheduled window's start — a re-schedule
//!   is a cancel plus a new window, because `WINDOW_START_IN_PAST`
//!   (`inst-ws-future-start`, D-63) bounds *creation* and would otherwise be
//!   evadable by creating a legal window and then moving its start backwards;
//! * a trigger that admitted a mutable future start would have to read the clock
//!   to know whether this particular start had passed, so the guard would be the
//!   one thing in the chain that answers differently depending on when it is
//!   asked.
//!
//! The direction is the safe one and the difference is a real one. Widening the
//! whitelist to admit a future start needs a decision, not a patch.
//!
//! # Non-overlap per canonical scope key is **not here**, and that is a choice
//!
//! §6 requires "non-overlap per canonical scope key enforced inside every
//! mutation". No **declarative** schema object can state it: the canonical scope
//! key is ten columns of `pricing_price` and this table carries `price_id` and
//! nothing else of it, so a `UNIQUE` index has no columns to name, a
//! partial-index predicate sees only its own row (neither the parent's key nor a
//! sibling window's interval), and a range exclusion constraint would need
//! `btree_gist` on Postgres, has no `SQLite` expression at all, and would still
//! be per-`price_id` rather than per-key.
//!
//! **A trigger could carry it, and saying otherwise would be overstating the
//! case.** `pricing_price_tier_band_parent_kind` in `pricing_price_tier_band` is
//! already a cross-table trigger in this chain: a `BEFORE INSERT OR UPDATE` arm
//! here could join `pricing_price` on the ten axes and scan the key's
//! occupying windows. It is not built, and the reason is scope rather than
//! impossibility — the rule needs the same key resolution, the same
//! occupying-state set and the same half-open arithmetic
//! [`window_repo`](crate::infra::storage::repo::window_repo) already holds, and a
//! second procedural spelling of it (PL/pgSQL plus a `SQLite` trigger body) would
//! be two more answers to which intervals collide.
//!
//! The residue that choice leaves is stated rather than implied: **non-overlap is
//! the only invariant of this table guarded at one layer instead of two**, so a
//! writer that reaches the table past
//! [`window_repo`](crate::infra::storage::repo::window_repo) — raw SQL, a
//! backfill, a future repository — meets nothing. That is the one guarantee a
//! reader must not read into the schema.
//!
//! # The trigger, and the arms
//!
//! The whitelist shape of `pricing_price` rather than an
//! unconditional ban, because most of what this table does is legal movement.
//! Five arms, in this order:
//!
//! 1. **`DELETE` is always refused.** "Cancel is a state, not a deletion" (§6,
//!    verbatim). There is no state, no lifecycle and no actor for which a window
//!    row may be removed: a cancelled window is the evidence that an operator
//!    unscheduled a price change, and an expired one is how a past instant is
//!    priced on replay.
//! 2. **An `UPDATE` of an `expired` or `cancelled` window is refused outright**
//!    — `inst-ws-immutable`'s "expired/cancelled windows are immutable history",
//!    which the transition arm alone does not give: a terminal row whose `state`
//!    stays put could otherwise still have its `effective_to` moved, rewriting
//!    the interval a billed period was priced over. `pricing_approval`'s
//!    "a decided record is immutable" arm is the same shape.
//! 3. **The frozen-column whitelist.** `window_id`, `tenant_id`, `price_id`,
//!    `effective_from`, `reason_code`, `created_by` and `created_at` are
//!    immutable — `price_id` because §6 makes the window/price binding immutable
//!    after creation (and with it the key the window is filed under),
//!    `effective_from` per the narrowing above. The **only** mutable columns are
//!    `state`, `effective_to` and the three flip timestamps.
//! 4. **The sanctioned transitions**, §4's three edges and no others.
//! 5. **`effective_to` may only be moved while it is in the future, and only to
//!    a future instant** — §6's "permitted UPDATEs: state-machine transitions,
//!    **future** `effective_to` adjustment". Both halves: a move *to* the past
//!    would reprice an interval that has already elapsed, and a move *of* an end
//!    that has already elapsed would resurrect coverage the key had lost. Moving
//!    `effective_to` to NULL is permitted — that is the open-ended extension of
//!    `inst-ws-expire`, and it removes no coverage.
//!
//! Membership is tested rather than change, as `pricing_plan_append_only` tests
//! it: a `NEW IS DISTINCT FROM OLD` conjunct on arm 4 would let the `SQLite`
//! mirror accept a no-op the Postgres branch refuses, and a backend divergence
//! is worse than the hole it would close.
//!
//! There is **no born-in-a-state `INSERT` guard**, and the honest reason is a
//! trade-off rather than the one first written here. `pricing_approval` refuses a
//! record born anything but `submitted` because a record born `approved` defeats
//! the two-person rule *by existing* — there is no `UPDATE` for the decision plane
//! to guard. Nothing here is defeated that way: a window born `active` covers
//! exactly the interval it would have covered had it been flipped there, and the
//! biconditionals plus the three ordering CHECKs above now refuse every incoherent
//! born state a row-local predicate can see.
//!
//! What actually forbids the guard is narrower and worth writing down, because the
//! earlier reason — "later groups seed mid-life windows as fixtures" — was not the
//! constraint: **arm 5's "an end that has already passed" half is only testable by
//! a born-in-a-state INSERT.** No sanctioned mutation can produce a row whose
//! `effective_to` is already behind the clock, so `postgres_window.rs` and
//! `sqlite_window_guards.rs` both seed one straight into that shape, and a born-`scheduled`
//! INSERT guard would take that statement — and with it the proof of half of an arm
//! — away from the suite that proves this table. The residue is the same one and it
//! is still named: a row born `expired` whose `expired_at` sits at or after its
//! `effective_to` and whose `activated_at` sits at or after its `effective_from`
//! records a lifecycle nobody drove, and nothing in this table refuses it.
//!
//! There is no `REVOKE`. It names a deployment role this migration does not own
//! and `SQLite` has no `GRANT`/`REVOKE` at all; the trigger is the portable half
//! of the discipline §6 calls "REVOKE + column-whitelist trigger discipline"
//! (see `pricing_plan`'s module doc).
//!
//! **Backend differences.** The systematic type mirror (`uuid` -> `text`,
//! `timestamptz` -> `text`), `now()` -> the RFC 3339 `strftime` its writers spell, and the trigger
//! split: Postgres carries one PL/pgSQL function interpolating the offending
//! values, while `SQLite` has no procedural language and `RAISE(ABORT, ...)`
//! takes a **literal** message only, so the five rules become five triggers with
//! fixed messages, `IS DISTINCT FROM` written `IS NOT`, and each arm's `WHEN`
//! carrying the terminal-state exclusion that arm 2's early return gives for
//! free on Postgres — so that exactly one of them can fire on any one statement,
//! in the same order.
//!
//! One further `SQLite` caveat is real rather than cosmetic, and it is the one this
//! mirror got **wrong** until 2026-08-04. Every instant is `text` on this backend,
//! so every comparison is lexicographic, and the rule that makes that safe has two
//! halves rather than one:
//!
//! * **Between two stored instants**, lexicographic comparison is exact — but
//!   **not for the reason this paragraph gave until 2026-08-18** (review Z2-8). It
//!   said the rendering is "fixed-width, zero-padded and monotonic". It is not
//!   fixed-width: `sqlx-sqlite` encodes a `OffsetDateTime` as
//!   `to_rfc3339_opts(SecondsFormat::AutoSi, false)`, and `AutoSi` picks 0, 3, 6 or
//!   9 fractional digits **per value**, so one column holds 25-, 29-, 32- and
//!   35-character renderings side by side. This gear's own writers mix them:
//!   `check_authored_instant` refuses an *authored* instant finer than a
//!   millisecond and its doc explicitly excludes storage bookkeeping, so
//!   `OffsetDateTime::now_utc()` reaches `created_at_utc`, `submitted_at`, `enqueued_at` and
//!   `recorded_at` at nanosecond precision.
//!
//!   The conclusion survives on a different argument, and it is the argument that
//!   belongs here: the **offset sign sorts below both the fraction separator and
//!   the digits** — `'+'` is 0x2B, `'.'` is 0x2E, `'0'`–`'9'` are 0x30–0x39. So
//!   `…:45+00:00` < `…:45.500+00:00` (`'+'` < `'.'`) and
//!   `…:45.500+00:00` < `…:45.500000001+00:00` (`'+'` < `'0'`), which is
//!   chronological order in both cases, and that holds for every pair of `AutoSi`
//!   renderings. On that ground `chk_pricing_price_window_interval`,
//!   `chk_pricing_price_window_activation_order`,
//!   `chk_pricing_price_window_expiry_order` and `list_for_plan`'s `ORDER BY` do all
//!   compare the columns directly. This is `pricing_price`'s `grandfather_until`
//!   caveat, one table over.
//!
//!   Why the false premise was worth correcting rather than leaving as a harmless
//!   overstatement: this sentence is the crate's **standing licence** to compare
//!   these columns directly, quoted by `pricing_migration` and by
//!   `pricing_price`, and "fixed-width" is exactly the premise that would license
//!   a `substr(…, 1, 25)` normalization — which would truncate a nanosecond
//!   rendering mid-fraction and reintroduce the class this file records having
//!   already shipped once. The residual cost is unchanged: a writer producing a
//!   *different* spelling silently breaks the ordering, which is why the mirror
//!   suite's fixtures are written in the deployed rendering and not in a legible
//!   one.
//! * **Against `CURRENT_TIMESTAMP`, it is not** — and arm 5 is the only place one
//!   side of a comparison is the clock. `CURRENT_TIMESTAMP` renders
//!   `YYYY-MM-DD HH:MM:SS`: a **space** at byte 11 where the stored text has `T`.
//!   The arm therefore normalizes with `datetime(…)`, which parses both spellings,
//!   applies the offset and yields the clock's own shape. The `substr(…, 1, 19)`
//!   prefix this arm carried before did not: byte 11 compared `'T'` (0x54) against
//!   `' '` (0x20), so **every stored instant on today's UTC date sorted greater
//!   than the clock and read as future**, and the arm was inoperative for exactly
//!   the same-day window. Cross-date it happened to be right, because the date
//!   prefix dominates before byte 11 is reached — which is why a suite of cross-date
//!   fixtures kept it green. `datetime(…)` truncates to whole seconds, so a
//!   sub-second-in-the-future end reads as *now* and the arm refuses it: that is a
//!   divergence from the Postgres branch's full-precision comparison, it is in the
//!   fail-closed direction, and a D-144 instant is quantized to the millisecond so
//!   the only affected window is one being moved within the current second.
//!
//! Postgres compares `timestamptz` values and needs none of this. The Postgres
//! `down` drops the function as well as the table; the `SQLite` one drops only the
//! table.
//!
//! `pricing_price_window` gains `mutation_seq` — the monotonic per-window counter
//! that names an **act** (D-190) and gives the surface an entity tag (D-191).
//!
//! Two owed items, one column, which is why they land together: D-190 needs
//! something monotonic to tell one act on a window from the next, and D-191's
//! `If-Match` needs something to compare an entity tag against. The window row
//! carried neither — unlike `pricing_price`, whose `row_version` is exactly what the
//! price routes' precondition compares.
//!
//! # It counts **acts**, not row writes, and that is load-bearing
//!
//! The two clock-driven edges of §4 — `inst-ws-activate` and `inst-ws-expire` —
//! leave the number alone; only an operator's act advances it (a schedule is born at
//! `0`, an `effectiveTo` adjustment and a cancellation each add one). That is not a
//! convenience and it is the one thing about this column a later group must not
//! "simplify":
//!
//! An act's identity is what an approval unit's subject is built from (D-184), and
//! the retry that follows an approve has to render the **same** subject the refused
//! attempt did. If the activation sweep advanced this counter, a window that reached
//! its `effective_from` between the refusal and the approved retry would make the
//! retry name a subject no unit was ever opened under — so it would find nothing,
//! open a second unit, and the approval loop would have no exit. That is precisely
//! the defect D-184 closed, arriving through the clock rather than through the
//! window id.
//! `tests/sqlite_window_repo.rs::the_activation_sweep_does_not_advance_the_act_sequence`
//! is the pin, and its doc carries this argument where a reader of the sweep will
//! meet it.
//!
//! The cost of that choice is stated rather than hidden: as an entity tag this
//! number tracks the **acts** on a window and not its whole representation, so a
//! window that activated carries the tag it had while `scheduled`. Nothing reads a
//! window through a `GET` (there is none — D-191 clause (2)), so no cache validator
//! depends on it; and the precondition it does serve is not weakened, because the
//! writing transaction re-reads the row and judges the adjustment against the
//! **stored** state through `refuse_frozen_end` whatever the caller's tag said.
//!
//! # A sixth arm on the trigger, and no CHECK
//!
//! The whitelist arm of `pricing_price_window` freezes columns by naming them, so a new
//! column is mutable by default — and an unconstrained counter is one `UPDATE` away
//! from being a counter that goes backwards, which is the one thing a monotonic name
//! must not do. The sixth arm therefore admits exactly two shapes: unchanged (the
//! sweep's flips) or `OLD + 1` (an act). A decrement, a skip and a reset are all
//! refused.
//!
//! `chk_pricing_price_window_mutation_seq` carries the floor the sixth arm cannot.
//! That arm is `BEFORE UPDATE` and says nothing about an INSERT, so an out-of-band
//! insert of a negative reaches the table and stays: [`WindowRecord`] converts the
//! column to a `u64` and answers [`RepoError::CorruptRow`], which is an internal
//! fault on **every** later read of that window rather than a fault the writer is
//! told about. The columns a `CorruptRow` reading can be poisoned through are
//! exactly the ones whose CHECK is missing.
//!
//! It costs a clause in each `CREATE TABLE` and nothing else. `SQLite` has no
//! `ADD CONSTRAINT`, which would matter to a chain that alters tables in place —
//! this one is squashed and re-issued, so a constraint is authored where the table
//! is, on both arms, and no rebuild is weighed against it.
//!
//! [`WindowRecord`]: crate::infra::storage::repo::window_repo::WindowRecord
//! [`RepoError::CorruptRow`]: crate::infra::storage::RepoError::CorruptRow
//!
//! # The hole this closes
//!
//! `window_repo::refuse_overlap` is a `SELECT` walk and its caller inserts
//! afterwards, with nothing in between: no lock, no advisory key, and — until
//! this migration — no constraint. Both indexes on `pricing_price_window` are
//! plain `CREATE INDEX`. Under `READ COMMITTED` two concurrent mutations on one
//! key therefore both read the key as free and both commit, which
//! `tests/postgres_window.rs` pinned as an assertion rather than a comment.
//!
//! `refuse_overlap` stays exactly where it is. It is the explanatory path — it
//! names the colliding window and renders the key — and the constraint is the
//! guarantee behind it, the same read-then-constraint arrangement `pricing_price`
//! already has on its scope key (D-148).
//!
//! # Scoped by `price_id`, and the residue is stated rather than hidden
//!
//! The rule `refuse_overlap` implements is per **canonical scope key**, and it
//! spans price rows: it gathers every row of the plan whose key equals this one
//! (`mates`) and walks all their windows. A superseded predecessor and its
//! published successor share a key and are two `price_id`s.
//!
//! A constraint can only see columns of its own table, and `pricing_price_window`
//! carries `price_id`, not the key. So this closes the **same-row** race — every
//! interactive schedule, adjust and cancel, which is where the contention
//! actually is — and leaves the **cross-mate** case to `refuse_overlap` alone.
//!
//! Closing that half needs the rendered scope key denormalised onto this table.
//! It is a sound target: the key is immutable once a row exists (the eight axes
//! are simply absent from `PriceContent`, and `check_update_keeps_the_line`
//! refuses a move of the `(meter, dimensionKey)` pair), so a copy cannot drift.
//! What it costs is a backfill that renders the key **in SQL**, and a rendering
//! that disagrees with `ScopeKey`'s `Display` by one separator would split the
//! key space silently — every row landing in its own partition, the constraint
//! green and enforcing nothing. That is a data migration owing its own proof,
//! not a clause of this one.
//!
//! # Postgres: `EXCLUDE USING gist`, partial over the occupying states
//!
//! ```sql
//! EXCLUDE USING gist (
//!     tenant_id WITH =,
//!     price_id  WITH =,
//!     tstzrange(effective_from, effective_to, '[)') WITH &&)
//! WHERE (state IN ('scheduled','active'))
//! ```
//!
//! The predicate is `domain::window::OCCUPYING_STATES` and nothing else: a
//! cancelled window never took effect and an expired one cannot be intersected
//! by anything `inst-ws-future-start` admits, so neither occupies its interval.
//! Without the `WHERE` this would refuse the ordinary act of cancelling a window
//! and scheduling its replacement over the same span.
//!
//! `tstzrange(from, to, '[)')` with a `NULL` upper bound is Postgres's own
//! open-ended range, so the open-ended window needs no sentinel timestamp; and
//! `[)` makes `effective_to = next.effective_from` **adjacency rather than
//! collision**, matching `window_repo::intersects` and this table's own
//! `chk_pricing_price_window_interval`.
//!
//! `EXCLUDE` needs `btree_gist` for the equality operators over `uuid`
//! (`tstzrange` has native `gist` support). The extension is
//! `m20260821_000002_install_btree_gist`'s, ahead of every table: it is a
//! database-level fact that neither of the two tables carrying an `EXCLUDE` owns,
//! and an owner among them would let a rollback of one table pull the floor out
//! from under the other's constraint.
//!
//! # `SQLite`: the same test as a pair of `RAISE(ABORT)` triggers
//!
//! No exclusion constraint exists there, so `BEFORE INSERT` and `BEFORE UPDATE`
//! spell the same NULL-safe half-open overlap by hand. The cross-row `EXISTS`
//! sits in the body as `SELECT RAISE(ABORT, …) WHERE EXISTS (…)`, which fires
//! when the subquery finds a collision and is silent otherwise — the body rather
//! than a `WHEN` clause for `pricing_bulk_row_lock`'s reason, which is a spelling and
//! not an engine limitation: a `SQLite` `WHEN` does accept a subquery, and
//! `pricing_approval_key` has four that do.
//!
//! The `UPDATE` arm excludes the row's own previous self
//! (`existing.window_id <> NEW.window_id`), because shortening a window is an
//! `UPDATE` of exactly the columns being compared and would otherwise collide
//! with itself. The `INSERT` arm needs no such exclusion.
//!
//! Both arms carry the state predicate on **both** sides — the existing row must
//! occupy, and the new one must too — because an `UPDATE` that cancels a window
//! is how a cancellation is written, and it must not be refused by the interval
//! it is vacating.
//!
//! # About this file
//!
//! Dependency level 1: everything it references is created before it.
//! Columns read identity first, then content by name, then the audit columns.
//!
//! The SQL is generated by `tasks/emit_chain.py` from the frozen schema goldens and
//! is rewritten on every run; this doc is not. What dissolved into this migration is
//! recorded in `tasks/migration-inventory.md`, which is where to look for the chain's
//! own history — nothing above narrates it, because a fresh-install chain has none.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE bss.pricing_price_window (
            tenant_id      uuid        NOT NULL,
            window_id      uuid        NOT NULL,
            activated_at   timestamptz,
            cancelled_at   timestamptz,
            effective_from timestamptz NOT NULL,
            effective_to   timestamptz,
            expired_at     timestamptz,
            mutation_seq   bigint      NOT NULL DEFAULT 0,
            price_id       uuid        NOT NULL,
            reason_code    text        NOT NULL,
            state          text        NOT NULL,
            created_at     timestamptz NOT NULL DEFAULT now(),
            created_by     uuid        NOT NULL,
            CONSTRAINT chk_pricing_price_window_activated_at CHECK ((state IN ('active','expired')) = (activated_at IS NOT NULL)),
            CONSTRAINT chk_pricing_price_window_activation_order CHECK (activated_at IS NULL OR activated_at >= effective_from),
            CONSTRAINT chk_pricing_price_window_cancelled_at CHECK ((state = 'cancelled') = (cancelled_at IS NOT NULL)),
            CONSTRAINT chk_pricing_price_window_expired_at CHECK ((state = 'expired') = (expired_at IS NOT NULL)),
            CONSTRAINT chk_pricing_price_window_expiry_order CHECK (expired_at IS NULL OR expired_at >= effective_to),
            CONSTRAINT chk_pricing_price_window_interval CHECK (effective_to IS NULL OR effective_to > effective_from),
            CONSTRAINT chk_pricing_price_window_open_ended CHECK (NOT (state = 'expired' AND effective_to IS NULL)),
            CONSTRAINT chk_pricing_price_window_reason_code CHECK (length(btrim(reason_code)) > 0),
            CONSTRAINT chk_pricing_price_window_mutation_seq CHECK (mutation_seq >= 0),
            CONSTRAINT chk_pricing_price_window_state CHECK (state IN ('scheduled','active','expired','cancelled')),
            CONSTRAINT excl_pricing_price_window_no_overlap EXCLUDE USING gist (tenant_id WITH =, price_id WITH =, tstzrange(effective_from, effective_to, '[)'::text) WITH &&) WHERE ((state = ANY (ARRAY['scheduled'::text, 'active'::text]))),
            CONSTRAINT fk_pricing_price_window_price FOREIGN KEY (price_id) REFERENCES bss.pricing_price(price_id),
            CONSTRAINT pricing_price_window_pkey PRIMARY KEY (window_id)
        )",
    "CREATE INDEX idx_pricing_price_window_due ON bss.pricing_price_window USING btree (state, effective_from)",
    "CREATE INDEX idx_pricing_price_window_price ON bss.pricing_price_window USING btree (tenant_id, price_id)",
    "CREATE OR REPLACE FUNCTION bss.pricing_price_window_append_only() RETURNS trigger AS $$
        BEGIN
          IF TG_OP = 'DELETE' THEN
            RAISE EXCEPTION
              'pricing_price_window: DELETE of window % is not permitted; cancel is a state, not a deletion',
              OLD.window_id;
          END IF;

          IF OLD.state IN ('expired','cancelled') THEN
            RAISE EXCEPTION
              'pricing_price_window: window % is %; an expired or cancelled window is immutable history',
              OLD.window_id, OLD.state;
          END IF;

          IF NEW.window_id      IS DISTINCT FROM OLD.window_id
          OR NEW.tenant_id      IS DISTINCT FROM OLD.tenant_id
          OR NEW.price_id       IS DISTINCT FROM OLD.price_id
          OR NEW.effective_from IS DISTINCT FROM OLD.effective_from
          OR NEW.reason_code    IS DISTINCT FROM OLD.reason_code
          OR NEW.created_by     IS DISTINCT FROM OLD.created_by
          OR NEW.created_at     IS DISTINCT FROM OLD.created_at THEN
            RAISE EXCEPTION
              'pricing_price_window: window % is bound to its price row and its start; only state, effective_to and the flip timestamps may move',
              OLD.window_id;
          END IF;

          IF NEW.state IS DISTINCT FROM OLD.state
             AND NOT (OLD.state = 'scheduled' AND NEW.state IN ('active','cancelled'))
             AND NOT (OLD.state = 'active'    AND NEW.state = 'expired') THEN
            RAISE EXCEPTION
              'pricing_price_window: state % -> % is not a sanctioned transition',
              OLD.state, NEW.state;
          END IF;

          IF NEW.effective_to IS DISTINCT FROM OLD.effective_to
             AND ((NEW.effective_to IS NOT NULL AND NEW.effective_to <= now())
               OR (OLD.effective_to IS NOT NULL AND OLD.effective_to <= now())) THEN
            RAISE EXCEPTION
              'pricing_price_window: the effective_to of window % may only be moved while it is in the future, and only to a future instant',
              OLD.window_id;
          END IF;

          IF NEW.mutation_seq IS DISTINCT FROM OLD.mutation_seq
             AND NEW.mutation_seq <> OLD.mutation_seq + 1 THEN
            RAISE EXCEPTION
              'pricing_price_window: the act sequence of window % moves by one act at a time, from % - it names an act and a name that can be reused or run backwards names nothing',
              OLD.window_id, OLD.mutation_seq;
          END IF;

          RETURN NEW;
        END;
     $$ LANGUAGE plpgsql",
    "CREATE TRIGGER trg_pricing_price_window_append_only BEFORE DELETE OR UPDATE ON bss.pricing_price_window FOR EACH ROW EXECUTE FUNCTION bss.pricing_price_window_append_only()",
];

const PG_DOWN_STATEMENTS: &[&str] = &[
    "DROP TABLE IF EXISTS bss.pricing_price_window",
    "DROP FUNCTION IF EXISTS bss.pricing_price_window_append_only()",
];

const SQLITE_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE pricing_price_window (
            tenant_id      text    NOT NULL,
            window_id      text    NOT NULL,
            activated_at   text,
            cancelled_at   text,
            effective_from text    NOT NULL,
            effective_to   text,
            expired_at     text,
            mutation_seq   integer NOT NULL DEFAULT 0,
            price_id       text    NOT NULL,
            reason_code    text    NOT NULL,
            state          text    NOT NULL,
            created_at     text    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00'),
            created_by     text    NOT NULL,
            PRIMARY KEY (window_id),
            CONSTRAINT chk_pricing_price_window_activated_at CHECK ((state IN ('active','expired')) = (activated_at IS NOT NULL)),
            CONSTRAINT chk_pricing_price_window_activation_order CHECK (activated_at IS NULL OR activated_at >= effective_from),
            CONSTRAINT chk_pricing_price_window_cancelled_at CHECK ((state = 'cancelled') = (cancelled_at IS NOT NULL)),
            CONSTRAINT chk_pricing_price_window_expired_at CHECK ((state = 'expired') = (expired_at IS NOT NULL)),
            CONSTRAINT chk_pricing_price_window_expiry_order CHECK (expired_at IS NULL OR expired_at >= effective_to),
            CONSTRAINT chk_pricing_price_window_interval CHECK (effective_to IS NULL OR effective_to > effective_from),
            CONSTRAINT chk_pricing_price_window_open_ended CHECK (NOT (state = 'expired' AND effective_to IS NULL)),
            CONSTRAINT chk_pricing_price_window_reason_code CHECK (length(trim(reason_code)) > 0),
            CONSTRAINT chk_pricing_price_window_mutation_seq CHECK (mutation_seq >= 0),
            CONSTRAINT chk_pricing_price_window_state CHECK (state IN ('scheduled','active','expired','cancelled')),
            CONSTRAINT fk_pricing_price_window_price FOREIGN KEY (price_id) REFERENCES pricing_price(price_id)
        )",
    "CREATE INDEX idx_pricing_price_window_due ON pricing_price_window (state, effective_from)",
    "CREATE INDEX idx_pricing_price_window_price ON pricing_price_window (tenant_id, price_id)",
    "CREATE TRIGGER trg_pricing_price_window_act_sequence BEFORE UPDATE ON pricing_price_window FOR EACH ROW WHEN OLD.state NOT IN ('expired','cancelled') AND NEW.mutation_seq IS NOT OLD.mutation_seq AND NEW.mutation_seq <> OLD.mutation_seq + 1 BEGIN SELECT RAISE(ABORT, 'pricing_price_window: the act sequence moves by one act at a time; it names an act, and a name that can be reused or run backwards names nothing'); END",
    "CREATE TRIGGER trg_pricing_price_window_flip_whitelist BEFORE UPDATE ON pricing_price_window FOR EACH ROW WHEN OLD.state NOT IN ('expired','cancelled') AND NEW.state IS NOT OLD.state AND NOT (OLD.state = 'scheduled' AND NEW.state IN ('active','cancelled')) AND NOT (OLD.state = 'active' AND NEW.state = 'expired') BEGIN SELECT RAISE(ABORT, 'pricing_price_window: state transition is not a sanctioned one'); END",
    "CREATE TRIGGER trg_pricing_price_window_frozen_columns BEFORE UPDATE ON pricing_price_window FOR EACH ROW WHEN OLD.state NOT IN ('expired','cancelled') AND (NEW.window_id IS NOT OLD.window_id OR NEW.tenant_id IS NOT OLD.tenant_id OR NEW.price_id IS NOT OLD.price_id OR NEW.effective_from IS NOT OLD.effective_from OR NEW.reason_code IS NOT OLD.reason_code OR NEW.created_by IS NOT OLD.created_by OR NEW.created_at IS NOT OLD.created_at) BEGIN SELECT RAISE(ABORT, 'pricing_price_window: the window is bound to its price row and its start; only state, effective_to and the flip timestamps may move'); END",
    "CREATE TRIGGER trg_pricing_price_window_future_end BEFORE UPDATE ON pricing_price_window FOR EACH ROW WHEN OLD.state NOT IN ('expired','cancelled') AND NEW.effective_to IS NOT OLD.effective_to AND ((NEW.effective_to IS NOT NULL AND datetime(NEW.effective_to) <= CURRENT_TIMESTAMP) OR (OLD.effective_to IS NOT NULL AND datetime(OLD.effective_to) <= CURRENT_TIMESTAMP)) BEGIN SELECT RAISE(ABORT, 'pricing_price_window: effective_to may only be moved while it is in the future, and only to a future instant'); END",
    "CREATE TRIGGER trg_pricing_price_window_immutable_history BEFORE UPDATE ON pricing_price_window FOR EACH ROW WHEN OLD.state IN ('expired','cancelled') BEGIN SELECT RAISE(ABORT, 'pricing_price_window: an expired or cancelled window is immutable history'); END",
    "CREATE TRIGGER trg_pricing_price_window_no_delete BEFORE DELETE ON pricing_price_window FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_price_window: DELETE of a window is not permitted; cancel is a state, not a deletion'); END",
    "CREATE TRIGGER trg_pricing_price_window_no_overlap_insert BEFORE INSERT ON pricing_price_window FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_price_window: interval overlaps an occupying window on this price row') WHERE NEW.state IN ('scheduled','active') AND EXISTS (SELECT 1 FROM pricing_price_window existing WHERE existing.tenant_id = NEW.tenant_id AND existing.price_id = NEW.price_id AND existing.state IN ('scheduled','active') AND (existing.effective_to IS NULL OR NEW.effective_from < existing.effective_to) AND (NEW.effective_to IS NULL OR existing.effective_from < NEW.effective_to)); END",
    "CREATE TRIGGER trg_pricing_price_window_no_overlap_update BEFORE UPDATE ON pricing_price_window FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_price_window: interval overlaps an occupying window on this price row') WHERE NEW.state IN ('scheduled','active') AND EXISTS (SELECT 1 FROM pricing_price_window existing WHERE existing.tenant_id = NEW.tenant_id AND existing.price_id = NEW.price_id AND existing.window_id <> NEW.window_id AND existing.state IN ('scheduled','active') AND (existing.effective_to IS NULL OR NEW.effective_from < existing.effective_to) AND (NEW.effective_to IS NULL OR existing.effective_from < NEW.effective_to)); END",
];

const SQLITE_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS pricing_price_window"];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        super::exec_backend(self.name(), manager, PG_UP_STATEMENTS, SQLITE_UP_STATEMENTS).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        super::exec_backend(
            self.name(),
            manager,
            PG_DOWN_STATEMENTS,
            SQLITE_DOWN_STATEMENTS,
        )
        .await
    }
}
