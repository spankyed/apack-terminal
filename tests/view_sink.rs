//! What is waiting to be written to one view connection: that it is handed out in order, that it has an
//! end, and that what ends it is the view not reading rather than the view having gone.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use apack_terminal::view::sink::Sink;

#[test]
fn messages_are_handed_out_in_the_order_they_were_queued_and_then_there_are_no_more() {
    let sink = Sink::new(|| {});
    assert!(sink.deliver(vec![1]));
    assert!(sink.deliver(vec![2, 2]));
    sink.finish();
    assert!(!sink.deliver(vec![3]), "queued after the last was said to have been");
    assert_eq!(sink.next(), Some(vec![1]));
    assert_eq!(sink.next(), Some(vec![2, 2]));
    assert_eq!(sink.next(), None);
}

/// A writer waiting for a message is woken by one arriving, and by there being no more to come.
#[test]
fn a_writer_waiting_is_woken_by_a_message_and_by_the_end() {
    let sink = Sink::new(|| {});
    let writer = std::thread::spawn({
        let sink = sink.clone();
        move || (sink.next(), sink.next())
    });
    assert!(sink.deliver(vec![7]));
    sink.finish();
    assert_eq!(writer.join().expect("the writer ends"), (Some(vec![7]), None));
}

/// **What may wait is counted in bytes, and what has been written no longer counts.** A view that reads
/// can be sent any amount over time; one that does not is cut off at the limit, with what it had waiting
/// dropped rather than kept.
#[test]
fn a_sink_holds_up_to_its_limit_and_is_closed_by_the_message_that_passes_it() {
    let said = Arc::new(AtomicBool::new(false));
    let hung_up = Arc::clone(&said);
    let sink = Sink::holding(10, move || hung_up.store(true, Ordering::SeqCst));
    assert!(sink.deliver(vec![0; 6]));
    assert!(sink.deliver(vec![0; 4]), "exactly the limit waits");
    // Read, so there is room again
    assert_eq!(sink.next().map(|message| message.len()), Some(6));
    assert!(sink.deliver(vec![0; 6]));
    assert_eq!(sink.handed_out(), 1);

    assert!(!said.load(Ordering::SeqCst), "hung up on while it was within what may wait");
    assert!(!sink.deliver(vec![0; 1]), "a byte more than may wait was taken");
    // **And its connection is closed there and then**, by whatever queued that message: nothing else
    // would be along to do it, the view having stopped reading and perhaps writing too
    assert!(said.load(Ordering::SeqCst));
    assert_eq!(sink.next(), None, "what was waiting for a view that does not read was kept");
    assert!(!sink.deliver(vec![0; 1]), "a closed sink took a message");
}

/// A sink closed because its writer could write no more is closed, and is not one whose view failed to
/// read: the view went, and what it asked before going is still to be done.
#[test]
fn a_sink_its_writer_closed_was_not_overrun() {
    let said = Arc::new(AtomicBool::new(false));
    let hung_up = Arc::clone(&said);
    let sink = Sink::new(move || hung_up.store(true, Ordering::SeqCst));
    assert!(sink.deliver(vec![1]));
    sink.close();
    assert!(!said.load(Ordering::SeqCst), "a view that went was hung up on as one that would not read");
    assert!(!sink.deliver(vec![2]));
    assert_eq!(sink.next(), None);
}
