//! What is queued for a session's program: why more is refused, said so that a session whose pty went
//! wrong is not taken for one that is over.

use std::io::Write;
use std::time::{Duration, Instant};

use apack_terminal::writer::{Input, Refused};

/// A bound on a wait for another thread, so a broken case fails rather than hangs.
const PATIENCE: Duration = Duration::from_secs(20);

/// A pty that takes nothing.
struct Refusing;

impl Write for Refusing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// **A pty that refused a write is not a session that ended.** Both take no more, and whoever sends is
/// told which: a view told "no such session" forgets the session, and this one is still there.
#[test]
fn input_refused_by_the_pty_is_told_apart_from_input_for_a_session_that_ended() {
    let broken = Input::start(Box::new(Refusing));
    // The first is queued, and written by the input's own thread; what is sent once that write has
    // failed is refused, however long the thread takes to get to it
    let waiting_since = Instant::now();
    let refused = loop {
        if let Err(refused) = broken.send(b"typed".to_vec()) {
            break refused;
        }
        assert!(waiting_since.elapsed() < PATIENCE, "a pty that takes nothing went on being written to");
        std::thread::yield_now();
    };
    assert_eq!(refused, Refused::Broken);

    let ended = Input::start(Box::new(std::io::sink()));
    ended.close();
    assert_eq!(ended.send(b"typed".to_vec()), Err(Refused::Closed));
}
