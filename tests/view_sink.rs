//! What is waiting to be written to one view connection: that it is handed out in order, that it has an
//! end, and that what ends it is the view not reading rather than the view having gone.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use std::io::Write;

use apack_terminal::view::sink::{Sink, WRITE_STEP};

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

    assert!(!said.load(Ordering::SeqCst), "hung up on while it was within what may wait");
    assert!(!sink.deliver(vec![0; 1]), "a byte more than may wait was taken");
    // **And its connection is closed there and then**, by whatever queued that message: nothing else
    // would be along to do it, the view having stopped reading and perhaps writing too
    assert!(said.load(Ordering::SeqCst));
    assert_eq!(sink.next(), None, "what was waiting for a view that does not read was kept");
    assert!(!sink.deliver(vec![0; 1]), "a closed sink took a message");
}

/// **What the connection has taken is what says a view is reading, and it is seen before a message is
/// over.** A message is written a step at a time, and each step is counted once the connection has taken
/// it: a view reading a large answer slowly is reading, and the count moves while it does.
#[test]
fn what_the_connection_takes_is_counted_while_a_message_is_still_being_written() {
    /// A connection that records, at each write it is handed, how much had been counted before it
    struct Watching<'a> {
        sink: &'a Sink,
        counted_at_each_write: Vec<u64>,
        taken: Vec<u8>,
    }
    impl Write for &mut Watching<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.counted_at_each_write.push(self.sink.written());
            self.taken.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let sink = Sink::new(|| {});
    assert_eq!(sink.written(), 0);
    let message: Vec<u8> = (0..WRITE_STEP * 3 + 5).map(|at| at as u8).collect();
    let mut connection = Watching { sink: &sink, counted_at_each_write: Vec::new(), taken: Vec::new() };
    sink.write_to(&mut connection, &message).expect("the message is written");

    assert_eq!(connection.taken, message, "the message was not written whole and in order");
    assert_eq!(sink.written(), message.len() as u64);
    let counted = &connection.counted_at_each_write;
    assert!(counted.len() > 1, "a message several steps long was written in one");
    assert!(
        counted.windows(2).all(|pair| pair[0] < pair[1]),
        "the count did not move between the writes of one message: {counted:?}"
    );
}

/// A connection that refuses a write leaves counted only what it took.
#[test]
fn a_write_the_connection_refuses_is_not_counted() {
    struct Refusing;
    impl Write for Refusing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink::new(|| {});
    assert!(sink.write_to(Refusing, &[0; 10]).is_err());
    assert_eq!(sink.written(), 0, "bytes the connection refused were counted as taken");
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
