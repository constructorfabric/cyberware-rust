//! The leader-election watch: mapping a `kube::runtime::watcher` stream to
//! leadership transitions (DESIGN.md §4.3).
//!
//! Status is watch-driven, not poll-driven: each active election runs a watcher
//! field-selected on `metadata.name`, so the stream carries only this one Lease and
//! delivers a holder change in milliseconds rather than up to a renewal interval
//! late. The stream is mapped in two pure stages, both unit-tested here:
//!
//! 1. [`classify_event`] reduces a `watcher::Event<Lease>` to a coarse
//!    [`WatchSignal`] — the object's holder, a vacancy, a re-list, or a boundary
//!    marker — collapsing the five event kinds to the four the state machine cares
//!    about.
//! 2. [`holder_transitions`] maps `(previous status, observed holder, our identity)`
//!    to the ordered [`LeaderStatus`] transitions to emit, deduplicated against the
//!    previous status so a healthy holder's repeated `renewTime` advance is not a
//!    transition.
//!
//! `Lagged` is never synthesised here: the K8s watch protocol has no "fell behind
//! by N" signal — a watcher that cannot keep up gets a `410 Gone` and re-lists,
//! which is [`WatchSignal::Relisted`] (→ `Reset`), not a fabricated count (§4.3).

use k8s_openapi::api::coordination::v1::Lease;
use kube::runtime::watcher::Event;

use cluster_sdk::leader::LeaderStatus;

/// A coarse classification of one watcher event (§4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchSignal {
    /// The Lease was applied (a live update or the re-list payload). Carries its
    /// `holderIdentity`, `None` when the holder is cleared or unset.
    Observed(Option<String>),
    /// The Lease object was deleted — the claim vacated; the acquire path handles
    /// the now-free name (§4.3).
    Vacated,
    /// The watch was re-established (a reconnect, typically a `410 Gone` on an
    /// expired `resourceVersion`). Emit `Reset`, then reconcile from the following
    /// [`WatchSignal::Observed`] (§4.3).
    Relisted,
    /// A boundary marker with no action of its own (the end of a re-list).
    Quiet,
}

/// Reduces a `watcher::Event<Lease>` to a [`WatchSignal`] (§4.3).
///
/// - `Apply` / `InitApply` → [`WatchSignal::Observed`] carrying the holder.
/// - `Delete` → [`WatchSignal::Vacated`].
/// - `Init` → [`WatchSignal::Relisted`] (the re-list has begun; emit `Reset`).
/// - `InitDone` → [`WatchSignal::Quiet`].
#[must_use]
pub fn classify_event(event: Event<Lease>) -> WatchSignal {
    match event {
        Event::Apply(lease) | Event::InitApply(lease) => WatchSignal::Observed(holder_of(&lease)),
        Event::Delete(_) => WatchSignal::Vacated,
        Event::Init => WatchSignal::Relisted,
        Event::InitDone => WatchSignal::Quiet,
    }
}

/// The `holderIdentity` of a Lease, or `None` when the spec or the field is absent.
#[must_use]
pub fn holder_of(lease: &Lease) -> Option<String> {
    lease
        .spec
        .as_ref()
        .and_then(|spec| spec.holder_identity.clone())
        .filter(|holder| !holder.is_empty())
}

/// Maps an observed holder to the ordered leadership transitions to emit, given the
/// `previous` status and `our` identity (§4.3).
///
/// Deduplicated against `previous`: a holder unchanged from our point of view emits
/// nothing (a healthy holder advancing only `renewTime` is not a transition). The
/// one two-element case is losing the claim to another holder — `Lost` then
/// `Follower`, so a consumer sees the distinct loss before the follower state.
///
/// A cleared holder (`None`) is treated as "someone must claim it": we emit `Lost`
/// only if we *were* the leader (our claim vanished), and otherwise nothing — the
/// acquire path, not this mapping, decides who takes a free Lease.
#[must_use]
pub fn holder_transitions(
    previous: LeaderStatus,
    holder: Option<&str>,
    our: &str,
) -> Vec<LeaderStatus> {
    match holder {
        // The holder is us: we hold (or still hold) the claim.
        Some(h) if h == our => match previous {
            LeaderStatus::Leader => Vec::new(),
            LeaderStatus::Follower | LeaderStatus::Lost => vec![LeaderStatus::Leader],
        },
        // A different holder owns the claim.
        Some(_) => match previous {
            // We just lost it to them: surface the loss, then the follower state.
            LeaderStatus::Leader => vec![LeaderStatus::Lost, LeaderStatus::Follower],
            LeaderStatus::Follower => Vec::new(),
            LeaderStatus::Lost => vec![LeaderStatus::Follower],
        },
        // The Lease is free (holder cleared/absent).
        None => match previous {
            // Our own claim vanished — that is a loss, even before we re-acquire.
            LeaderStatus::Leader => vec![LeaderStatus::Lost],
            // A follower observing a vacancy stays a follower until the acquire
            // path wins the claim; already-Lost stays Lost pending re-enrollment.
            LeaderStatus::Follower | LeaderStatus::Lost => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{WatchSignal, classify_event, holder_transitions};
    use cluster_sdk::leader::LeaderStatus::{Follower, Leader, Lost};
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use kube::runtime::watcher::Event;

    fn lease_held_by(holder: Option<&str>) -> Lease {
        Lease {
            spec: Some(LeaseSpec {
                holder_identity: holder.map(str::to_owned),
                ..LeaseSpec::default()
            }),
            ..Lease::default()
        }
    }

    #[test]
    fn apply_and_init_apply_observe_the_holder() {
        assert_eq!(
            classify_event(Event::Apply(lease_held_by(Some("broker-7")))),
            WatchSignal::Observed(Some("broker-7".to_owned()))
        );
        assert_eq!(
            classify_event(Event::InitApply(lease_held_by(Some("broker-7")))),
            WatchSignal::Observed(Some("broker-7".to_owned()))
        );
        // A cleared/empty holder observes as None.
        assert_eq!(
            classify_event(Event::Apply(lease_held_by(None))),
            WatchSignal::Observed(None)
        );
        assert_eq!(
            classify_event(Event::Apply(lease_held_by(Some("")))),
            WatchSignal::Observed(None)
        );
    }

    #[test]
    fn delete_init_and_initdone_map_to_their_signals() {
        assert_eq!(
            classify_event(Event::Delete(lease_held_by(Some("x")))),
            WatchSignal::Vacated
        );
        assert_eq!(classify_event(Event::Init), WatchSignal::Relisted);
        assert_eq!(classify_event(Event::InitDone), WatchSignal::Quiet);
    }

    #[test]
    fn becoming_the_holder_emits_leader_once() {
        assert_eq!(holder_transitions(Follower, Some("me"), "me"), vec![Leader]);
        assert_eq!(holder_transitions(Lost, Some("me"), "me"), vec![Leader]);
        // Already leader and still holding: no transition (renewTime-only advance).
        assert_eq!(
            holder_transitions(Leader, Some("me"), "me"),
            Vec::<_>::new()
        );
    }

    #[test]
    fn losing_to_another_holder_emits_lost_then_follower() {
        assert_eq!(
            holder_transitions(Leader, Some("them"), "me"),
            vec![Lost, Follower]
        );
        // A follower seeing a (still) foreign holder: nothing new.
        assert_eq!(
            holder_transitions(Follower, Some("them"), "me"),
            Vec::<_>::new()
        );
        // From Lost, resolving to a foreign holder settles us as Follower.
        assert_eq!(holder_transitions(Lost, Some("them"), "me"), vec![Follower]);
    }

    #[test]
    fn a_vacated_lease_loses_only_if_we_held_it() {
        // Our claim vanished → Lost.
        assert_eq!(holder_transitions(Leader, None, "me"), vec![Lost]);
        // A follower/lost observer waits for the acquire path; no transition here.
        assert_eq!(holder_transitions(Follower, None, "me"), Vec::<_>::new());
        assert_eq!(holder_transitions(Lost, None, "me"), Vec::<_>::new());
    }
}
