//! A client's queue: what it takes, what it hands on, and what becomes of what it will not write.
//!
//! What is owed for a message — the credit its output spent, the word of whoever waits for the writer to
//! reach it — is owed whether or not it is written, so every case here that drops one asks whether it
//! was accounted for. A place in the queue (`Outgoing::reached`) is how that is seen from outside.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use apack_terminal::feed::{FEED_BYTES, Feed, feed_bytes};
use apack_terminal::output::Outgoing;

/// Long enough that a thread which was going to do something has done it.
const PATIENCE: Duration = Duration::from_secs(10);

/// A feed that holds `limit` bytes, and how many times it has hung up.
fn holding(limit: usize) -> (Arc<Feed>, Arc<AtomicUsize>) {
    let hung_up = Arc::new(AtomicUsize::new(0));
    let feed = Feed::holding(limit, {
        let hung_up = Arc::clone(&hung_up);
        move || {
            hung_up.fetch_add(1, Ordering::SeqCst);
        }
    });
    (feed, hung_up)
}

/// A message with nothing to write, and what is told once it has been accounted for.
fn place() -> (Outgoing, Receiver<()>) {
    let (reached, told) = channel();
    (Outgoing::reached(reached), told)
}

fn line(feed: &Feed, text: &str) -> (bool, Receiver<()>) {
    let (message, told) = place();
    (feed.deliver(Some(text.to_owned()), message), told)
}

fn accounted_for(told: &Receiver<()>) -> bool {
    told.try_recv().is_ok()
}

#[test]
fn what_is_delivered_is_handed_on_in_the_order_it_came_and_owed_nothing_until_it_is_written() {
    let (feed, _) = holding(100);
    let (taken, first) = line(&feed, "one");
    assert!(taken);
    assert!(line(&feed, "two").0);

    let (frame, message) = feed.next().expect("the first");
    assert_eq!(frame.as_deref(), Some("one"));
    // Handing a message on is not writing it: that is the writer's to say
    assert_eq!(first.try_recv(), Err(TryRecvError::Empty));
    message.written();
    assert!(accounted_for(&first));
    assert_eq!(feed.next().expect("the second").0.as_deref(), Some("two"));
}

#[test]
fn a_feed_takes_exactly_what_it_may_hold_and_a_byte_more_ends_it() {
    let (feed, hung_up) = holding(10);
    let (taken, first) = line(&feed, "1234");
    assert!(taken);
    let (taken, second) = line(&feed, "567890");
    assert!(taken, "ten bytes are what it may hold");
    assert_eq!(hung_up.load(Ordering::SeqCst), 0);

    let (taken, third) = line(&feed, "1");
    assert!(!taken, "an eleventh byte was taken");
    assert_eq!(hung_up.load(Ordering::SeqCst), 1, "the connection is closed where it is overrun, once");
    // Nothing of it will be written, and all of it is accounted for
    assert!(accounted_for(&first) && accounted_for(&second) && accounted_for(&third));
    assert!(feed.next().is_none());
}

#[test]
fn what_has_been_handed_on_is_no_longer_waiting() {
    let (feed, hung_up) = holding(10);
    assert!(line(&feed, "1234567890").0);
    feed.next().expect("the ten bytes");
    assert!(line(&feed, "1234567890").0, "ten more, with nothing waiting");
    assert_eq!(hung_up.load(Ordering::SeqCst), 0);
    // And what is still waiting counts: the ten there now leave no room
    assert!(!line(&feed, "1").0);
}

/// A place in the queue is nothing to write, so it takes no room however full the feed is.
#[test]
fn a_place_in_the_queue_takes_no_room() {
    let (feed, hung_up) = holding(4);
    assert!(line(&feed, "1234").0);
    let (message, told) = place();
    assert!(feed.deliver(None, message));
    assert_eq!(hung_up.load(Ordering::SeqCst), 0);
    feed.next().expect("the line");
    let (frame, message) = feed.next().expect("the place");
    assert_eq!(frame, None);
    message.written();
    assert!(accounted_for(&told));
}

#[test]
fn a_closed_feed_takes_nothing_and_accounts_for_what_it_is_given_and_what_it_held() {
    let (feed, hung_up) = holding(100);
    let (_, held) = line(&feed, "held");
    feed.close();
    assert!(accounted_for(&held), "what was waiting when it closed");
    assert!(feed.next().is_none());

    let (taken, late) = line(&feed, "late");
    assert!(!taken);
    assert!(accounted_for(&late), "what arrived after");
    // Closing is not hanging up: whoever closed it decides what becomes of the connection
    assert_eq!(hung_up.load(Ordering::SeqCst), 0);
}

#[test]
fn a_writer_waits_for_something_to_write_and_is_told_when_there_will_be_nothing() {
    let (feed, _) = holding(100);
    let (handed, taken) = channel();
    let writer = std::thread::spawn({
        let feed = Arc::clone(&feed);
        move || {
            while let Some((frame, message)) = feed.next() {
                message.written();
                handed.send(frame).expect("the case is listening");
            }
        }
    });

    assert!(line(&feed, "first").0);
    assert_eq!(taken.recv_timeout(PATIENCE).expect("the writer was woken").as_deref(), Some("first"));
    assert!(line(&feed, "second").0);
    assert_eq!(taken.recv_timeout(PATIENCE).expect("and woken again").as_deref(), Some("second"));

    // With nothing waiting it is asleep, and closing is what ends it
    feed.close();
    writer.join().expect("the writer ended");
    assert!(taken.try_recv().is_err());
}

/// A client may leave unread what one may in use unless a broker is told a number of bytes.
#[test]
fn the_most_a_client_may_leave_unread_is_the_one_in_use_unless_a_number_of_bytes_is_given() {
    assert_eq!(feed_bytes(None), FEED_BYTES);
    assert_eq!(feed_bytes(Some("65536")), 65536);
    assert_eq!(feed_bytes(Some("plenty")), FEED_BYTES);
    assert_eq!(feed_bytes(Some("")), FEED_BYTES);
}
