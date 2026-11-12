//! When a wait that nothing has met is given up on, and that one that was met is not.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use apack_terminal::deadlines::Deadlines;
use apack_terminal::output::Outgoing;
use apack_terminal::protocol::{Event, WaitOutcome};

/// Long enough that a deadline which was going to fall due has.
const PATIENCE: Duration = Duration::from_secs(10);

/// The session the next wait to be given up on was for.
fn given_up_on(told: &Receiver<Outgoing>) -> String {
    let message = told.recv_timeout(PATIENCE).expect("a wait was given up on");
    match message.event {
        Some(Event::Waited { session, outcome: WaitOutcome::Timeout }) => session,
        other => panic!("expected a wait to have timed out, was told {other:?}"),
    }
}

fn unsettled() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn a_wait_nothing_met_is_given_up_on_once_and_not_before_it_asked_to_be() {
    let (events, told) = channel();
    let deadlines = Deadlines::keeping(events);
    let settled = unsettled();
    let asked = Instant::now();
    let after = Duration::from_millis(150);
    deadlines.set(after, "s-1".to_owned(), Arc::clone(&settled));

    assert_eq!(given_up_on(&told), "s-1");
    assert!(asked.elapsed() >= after, "given up on after {:?}, having asked for {after:?}", asked.elapsed());
    assert!(settled.load(Ordering::SeqCst), "the wait is settled by whoever answers it");
    // And is not spoken of again: the next thing heard is the next wait
    deadlines.set(Duration::from_millis(50), "s-2".to_owned(), unsettled());
    assert_eq!(given_up_on(&told), "s-2");
}

#[test]
fn waits_are_given_up_on_as_each_falls_due_whatever_order_they_were_asked_in() {
    let (events, told) = channel();
    let deadlines = Deadlines::keeping(events);
    deadlines.set(Duration::from_millis(300), "last".to_owned(), unsettled());
    deadlines.set(Duration::from_millis(60), "first".to_owned(), unsettled());
    deadlines.set(Duration::from_millis(180), "second".to_owned(), unsettled());

    assert_eq!([given_up_on(&told), given_up_on(&told), given_up_on(&told)], ["first", "second", "last"]);
}

#[test]
fn a_wait_that_was_met_first_is_not_given_up_on() {
    let (events, told) = channel();
    let deadlines = Deadlines::keeping(events);
    let met = unsettled();
    deadlines.set(Duration::from_millis(60), "met".to_owned(), Arc::clone(&met));
    deadlines.set(Duration::from_millis(200), "unmet".to_owned(), unsettled());
    // Met where the bytes landed, before its deadline
    met.store(true, Ordering::SeqCst);

    // The first thing said is of the wait that fell due later
    assert_eq!(given_up_on(&told), "unmet");
}
