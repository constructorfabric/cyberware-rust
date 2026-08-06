//! Pure: a set behind a mutex. No runtime, no storage.

use uuid::Uuid;

use super::lease::{InProcessStreamLeases, StreamLease, StreamLeases};

#[test]
fn a_free_subscription_can_be_leased() {
    let leases = InProcessStreamLeases::new();
    let id = Uuid::new_v4();

    let lease = leases.acquire(id);

    assert!(lease.is_some());
    assert!(leases.is_held(id));
}

#[test]
fn a_second_acquire_is_refused_while_the_first_is_held() {
    let leases = InProcessStreamLeases::new();
    let id = Uuid::new_v4();
    let _first = leases.acquire(id).expect("first acquire");

    // This is the `409 StreamingInProgress` case, and every call carrying the
    // id except DELETE asks the same question.
    assert!(leases.acquire(id).is_none());
    assert!(leases.is_held(id));
}

#[test]
fn dropping_the_lease_releases_it() {
    let leases = InProcessStreamLeases::new();
    let id = Uuid::new_v4();

    {
        let _lease = leases.acquire(id).expect("acquire");
        assert!(leases.is_held(id));
    }

    // Release is ownership, not a separate guard somebody has to remember to
    // run - which is what made the previous marker's correctness depend on a
    // handler not destructuring its return value.
    assert!(!leases.is_held(id));
    assert!(leases.acquire(id).is_some());
}

#[test]
fn leases_are_per_subscription() {
    let leases = InProcessStreamLeases::new();
    let one = Uuid::new_v4();
    let two = Uuid::new_v4();

    let _one = leases.acquire(one).expect("acquire one");

    // Bound, not asserted on inline: a lease released inside the expression
    // that created it would be gone before the count is taken.
    let two_lease = leases.acquire(two);

    assert!(
        two_lease.is_some(),
        "a different subscription is unaffected"
    );
    assert_eq!(leases.held_count(), 2);
}

#[test]
fn an_unheld_subscription_reports_unheld() {
    let leases = InProcessStreamLeases::new();

    assert!(!leases.is_held(Uuid::new_v4()));
    assert_eq!(leases.held_count(), 0);
}

#[test]
fn the_lease_names_its_subscription() {
    let leases = InProcessStreamLeases::new();
    let id = Uuid::new_v4();

    let lease = leases.acquire(id).expect("acquire");

    assert_eq!(lease.subscription_id(), id);
}

#[test]
fn releasing_one_lease_does_not_release_another() {
    let leases = InProcessStreamLeases::new();
    let one = Uuid::new_v4();
    let two = Uuid::new_v4();
    let held = leases.acquire(one).expect("acquire one");
    let _kept = leases.acquire(two).expect("acquire two");

    drop(held);

    assert!(!leases.is_held(one));
    assert!(leases.is_held(two));
}

#[test]
fn concurrent_acquires_yield_exactly_one_lease() {
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    let leases = Arc::new(InProcessStreamLeases::new());
    let id = Uuid::new_v4();
    let gate = Arc::new(Barrier::new(16));

    // Checked and taken in one critical section, so a race cannot let two
    // callers both see the subscription free.
    //
    // The leases are *returned* rather than counted inside the thread. A lease
    // dropped at the end of the thread's body releases before the next thread
    // tries, so every caller wins in turn - which is the RAII behaviour working
    // correctly, and would make this test assert nothing.
    let held: Vec<Option<StreamLease>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let leases = Arc::clone(&leases);
                let gate = Arc::clone(&gate);
                scope.spawn(move || {
                    gate.wait();
                    leases.acquire(id)
                })
            })
            .collect();
        let mut leases = Vec::with_capacity(handles.len());
        for handle in handles {
            leases.push(handle.join().expect("thread"));
        }
        leases
    });
    let winners = held.iter().filter(|lease| lease.is_some()).count();

    assert_eq!(winners, 1, "exactly one caller may hold the lease");
}
